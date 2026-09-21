//! Which pages of a file the catalog's trees reach, and whether the free map
//! agrees.
//!
//! Invariant: **every page of the file below its page count is held by exactly
//! one thing.** `PagedTree::check` is about one tree read on its own and
//! `check_indexes_agree` is about a table and its indexes; neither can see a
//! page two trees both reach, because each tree is a well formed tree and
//! neither is an index of the other. That state loses rows with no symptom at
//! the time: two owners write plausible bytes into one page and the second
//! write is the first one's data gone.
//!
//! The check is a read. It reports the first thing that is wrong and repairs
//! nothing, for the reason [`crate::engine::integrity`] gives.
//!
//! ## Three states, and why only two of them reach `PRAGMA integrity_check`
//!
//! The walk can answer three questions, and the third one is held back:
//!
//! 1. **a page two things hold** - corruption, with no benign cause;
//! 2. **a page a tree holds that the free map calls free** - corruption with no
//!    benign cause either, and the state that *becomes* the first one at the
//!    next allocation;
//! 3. **a page the free map calls allocated that no tree reaches** - a leak,
//!    which the pinned SQLite reports as `Page N: never used`.
//!
//! **The engine produces the third state itself, from two ordinary statements**
//! (task-2052, measured over nineteen workloads, each read live and again after
//! a checkpoint and a reopen):
//!
//! - **a rolled-back `CREATE TABLE` or `CREATE INDEX` keeps its tree's root
//!   page.** The undo is row-level - `undo_to_floor` replays before-images
//!   through `tree.put` and `tree.delete` - so nothing gives the allocation
//!   back. It is invisible on the connection that did it, because the rollback
//!   leaves the tree's handle in `schema.trees` and this walk therefore still
//!   reaches the page; it appears after a reopen, when the schema is rebuilt
//!   from the catalog and nothing names that tree.
//! - **`DROP TABLE` keeps every page the table's out-of-line values sit on.**
//!   `release_tree` records [`inillucent_tree::PagedTree::pages`], which is the
//!   interior pages and the leaves. `paged::free_extent` is reached only from
//!   the tree's own write paths - a row deleted, a value replaced, two leaves
//!   merged - so a tree released whole never reaches it. `DELETE FROM t` before
//!   the drop gives the space back, which is why a drop of a table of small
//!   values is sound.
//!
//! **Both are leaks to fix, and neither is fixed here, which is the decision
//! this ticket was asked to record.** Either fix changes *when* the engine
//! hands a page back, and that is what task-2043 got subtly wrong: it freed a
//! dropped tree's pages at the statement rather than at the commit, a later
//! `CREATE TABLE` in the same transaction was given one of them, and the
//! rollback left a catalog row naming a page another table then held. Three
//! rows were lost durably and `PRAGMA integrity_check` answered `ok` about the
//! file, which is why this module exists at all. Shipping the detector and a
//! change to the same free map in one diff would mean the detector had never
//! been run against the engine as it was when that defect was found.
//!
//! So `PRAGMA integrity_check` reports the first two states and not the third.
//! [`crate::ImportedDatabase::report_leaked_pages`] is the third, written and
//! tested against a database damaged on purpose, so the arm cannot rot while
//! the two leaks are open. task-2065 closes them and wires it.

use std::collections::BTreeMap;

use inillucent_base::error::refusal;
use inillucent_base::{DbError, DbResult};
use inillucent_pool::{Database, PageId};
use inillucent_tree::{PageShare, PagedTree};

/// What holds a page.
///
/// **A handle rather than a name, because there is one of these per page.** A
/// 25 GB file at the default page size has eight hundred thousand of them, and
/// carrying the tree's name here meant cloning a `String` for every one. The
/// names are held once per tree in the map [`Holder::describe`] takes, and are
/// only read when there is something to report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Holder {
    /// One of the two meta pages, which are allocated from the moment they
    /// exist so that nothing can hand them out.
    Meta,
    /// A page of the free map's own chain.
    FreeMap,
    /// The tree this connection reads through that handle.
    Tree(u32),
    /// A shared extent page, which several trees legitimately write onto.
    SharedExtent,
}

impl Holder {
    /// Returns the phrase a message puts after "is used by".
    ///
    /// @param names - what each tree handle is called, one entry per tree
    fn describe(self, names: &BTreeMap<u32, String>) -> String {
        match self {
            Holder::Meta => "the meta record".to_string(),
            Holder::FreeMap => "the free map".to_string(),
            Holder::Tree(handle) => names
                .get(&handle)
                .cloned()
                .unwrap_or_else(|| format!("a tree this connection reads as {handle}")),
            Holder::SharedExtent => "a shared extent page".to_string(),
        }
    }
}

/// A corruption whose text reaches `PRAGMA integrity_check`'s row.
///
/// The pragma reports `detail` and falls back to the message, so a report that
/// set only one of them answered with the other's wording. The same shape as
/// `corrupt_index`, which does this for the index checks.
///
/// @param said - the sentence describing the damage
fn damaged(said: String) -> DbError {
    inillucent_base::error::corrupt(said.clone()).with_detail(said)
}

impl crate::ImportedDatabase {
    /// Reports the first page two things both hold, or that a tree holds and
    /// the free map has handed back.
    ///
    /// What `PRAGMA integrity_check` runs. Run per file, because a page number
    /// means nothing without one: `main` and an attached database both have a
    /// page 7.
    pub(crate) fn check_page_ownership(&self) -> DbResult<()> {
        self.walk_every_page(false)
    }

    /// Reports the first page the free map calls allocated that no tree
    /// reaches.
    ///
    /// **No pragma runs this**, and the module comment says why: the engine
    /// leaves that state behind itself, from a rolled-back `CREATE` and from a
    /// `DROP TABLE` of a table holding out-of-line values. Both are leaks to
    /// fix rather than states to live with, and either fix changes when a page
    /// is handed back - which is a change to the write path and not to a
    /// pragma's answers.
    ///
    /// It is public and tested so the arm is exercised against a database
    /// damaged on purpose, which is what stops it rotting while the two leaks
    /// are open. task-2065 closes them, and wiring this to the pragma is the
    /// `false` in `check_page_ownership` above.
    pub fn report_leaked_pages(&self) -> DbResult<()> {
        self.walk_every_page(true)
    }

    /// Walks every file's pages and reports the first thing that is wrong.
    ///
    /// @param leaks - whether a page nothing reaches is reported too
    fn walk_every_page(&self, leaks: bool) -> DbResult<()> {
        for (at, handles) in self.trees_by_schema() {
            let file = self
                .schema_file(at)
                .ok_or_else(|| refusal("a tree names a database that is not attached"))?;
            let names = self.name_every_tree(&handles);
            let held = self.hold_every_page(file, &handles, &names)?;
            compare_against_the_free_map(
                file,
                &held,
                &names,
                leaks,
                self.schema.skipped.is_empty(),
            )?;
        }
        Ok(())
    }

    /// Returns what each of one file's trees is called, one entry per tree.
    ///
    /// Built once per file so that a message can name a holder without every
    /// page having carried the name to get there.
    ///
    /// @param handles - the trees of that file, as this connection names them
    fn name_every_tree(&self, handles: &[u32]) -> BTreeMap<u32, String> {
        let mut names = BTreeMap::new();
        for handle in handles {
            let Some(tree) = self.schema.trees.get(handle) else {
                continue;
            };
            names.insert(*handle, self.name_of_tree(*handle, tree));
        }
        names
    }

    /// Groups this connection's tree handles by the file their pages live in.
    ///
    /// The catalog tree of every file is among them - it is registered under a
    /// handle of its own, `SCHEMA_VIEW_ROOT` for `main` and `catalog_handle`
    /// for an attached database - so a file's own schema is walked like any
    /// other tree rather than being the one thing the check cannot see.
    fn trees_by_schema(&self) -> BTreeMap<usize, Vec<u32>> {
        let mut grouped: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
        for handle in self.schema.trees.keys() {
            // **An imposter is a second handle over a tree that already has
            // one**, on purpose: `.imposter` declares a table over an index's
            // own b-tree so a person can read what the index holds, and its
            // own doc comment says "two declarations over one tree". Walking
            // both would report every page of that index as a page two things
            // hold, over a connection with nothing wrong with it.
            if self
                .schema
                .imposters
                .iter()
                .any(|(info, _, _)| info.root == *handle)
            {
                continue;
            }
            grouped
                .entry(self.session_state.schema_of(*handle))
                .or_default()
                .push(*handle);
        }
        for handles in grouped.values_mut() {
            handles.sort_unstable();
        }
        grouped
    }

    /// Builds the map from page to holder for one file, refusing a second
    /// holder.
    ///
    /// @param file - the database the pages belong to
    /// @param handles - the trees of that file, as this connection names them
    fn hold_every_page(
        &self,
        file: &Database,
        handles: &[u32],
        names: &BTreeMap<u32, String>,
    ) -> DbResult<BTreeMap<PageId, Holder>> {
        let mut held: BTreeMap<PageId, Holder> = BTreeMap::new();
        for page in 0..inillucent_pool::meta::FIRST_DATA_PAGE.0 {
            claim(&mut held, PageId(page), Holder::Meta, names)?;
        }
        for (page, _) in file.free_map_pages() {
            claim(&mut held, page, Holder::FreeMap, names)?;
        }
        for handle in handles {
            let Some(tree) = self.schema.trees.get(handle) else {
                continue;
            };
            for (page, share) in tree.pages_occupied(file.pool())? {
                let holder = match share {
                    // A shared extent page holds small values from whichever
                    // trees wrote them, because the hint that finds one is on
                    // the file and not on the tree. Several trees naming one is
                    // how the format works, so it is claimed under a holder of
                    // its own that cannot collide with anything.
                    PageShare::Shared => Holder::SharedExtent,
                    PageShare::Owned => Holder::Tree(*handle),
                };
                claim(&mut held, page, holder, names)?;
            }
        }
        Ok(held)
    }

    /// Names a tree the way the catalog does, for a message.
    ///
    /// A tree with no catalog row of its own - the file's own
    /// `sqlite_schema`, and any handle the tables have not been rebuilt from
    /// yet - is named by the page its root sits on, which is what identifies
    /// it in the file.
    ///
    /// @param handle - the handle this connection reads the tree through
    /// @param tree - the tree itself, for its root page
    fn name_of_tree(&self, handle: u32, tree: &PagedTree) -> String {
        for table in &self.schema.tables {
            if table.root == handle {
                return format!("table {}", String::from_utf8_lossy(&table.name));
            }
            for index in &table.indexes {
                if index.root == handle {
                    return format!("index {}", String::from_utf8_lossy(&index.name));
                }
            }
        }
        format!("the tree rooted at page {}", tree.root().0)
    }
}

/// Compares the pages the trees reach against what the free map says.
///
/// **A page the trees do not reach is only reported when this connection read
/// the whole schema.** An object whose `CREATE` text the engine could not
/// re-read is skipped at open and its tree is never attached, so its pages are
/// reachable from the file and not from here - and calling them unused would
/// report a file that is entirely sound as damaged.
///
/// @param file - the database the pages belong to
/// @param held - what each page the trees reach is held by
/// @param names - what each tree handle is called
/// @param leaks - whether a page nothing reaches is reported
/// @param complete - whether every object in the catalog was attached
fn compare_against_the_free_map(
    file: &Database,
    held: &BTreeMap<PageId, Holder>,
    names: &BTreeMap<u32, String>,
    leaks: bool,
    complete: bool,
) -> DbResult<()> {
    for number in inillucent_pool::meta::FIRST_DATA_PAGE.0..file.pool().page_count() {
        let page = PageId(number);
        match (held.get(&page), file.page_is_allocated(page)) {
            // A page a tree reaches that the map has handed back is the state
            // that becomes a two-owner page at the next allocation, and there
            // is nothing yet to see in either tree.
            (Some(holder), false) => {
                return Err(damaged(format!(
                    "page {number} is used by {} but the free map says it is free",
                    holder.describe(names)
                )))
            }
            // **The pinned reference's own wording, read out of its source
            // rather than remembered.** SQLite 3.53.4 writes `Page %u: never
            // used` (`sqlite3.c`, `sqlite3BtreeIntegrityCheck`); `Page N is
            // never used` is the older form and is what task-2052 asked for.
            // An application matching on this answer is matching on the text,
            // so it is the text the reference this repository grades against
            // produces today.
            (None, true) if leaks && complete => {
                return Err(damaged(format!("Page {number}: never used")))
            }
            _ => {}
        }
    }
    Ok(())
}

/// Records a page's holder, refusing a page that already has one.
///
/// **A shared extent page is the one page several holders may claim**, and the
/// two claims are equal rather than merely compatible: every tree that puts a
/// small value on one claims it as [`Holder::SharedExtent`], so the equality
/// below is what lets them through and nothing else.
///
/// @param held - what each page is held by so far
/// @param page - the page being claimed
/// @param holder - what is claiming it
/// @param names - what each tree handle is called, for the message
fn claim(
    held: &mut BTreeMap<PageId, Holder>,
    page: PageId,
    holder: Holder,
    names: &BTreeMap<u32, String>,
) -> DbResult<()> {
    match held.get(&page) {
        Some(existing) if *existing == holder && holder == Holder::SharedExtent => Ok(()),
        Some(existing) => Err(damaged(format!(
            "page {} is used by {} and also by {}",
            page.0,
            existing.describe(names),
            holder.describe(names)
        ))),
        None => {
            held.insert(page, holder);
            Ok(())
        }
    }
}
