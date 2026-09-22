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
//! ## Three states, and all three now reach `PRAGMA integrity_check`
//!
//! The walk answers three questions:
//!
//! 1. **a page two things hold** - corruption, with no benign cause;
//! 2. **a page a tree holds that the free map calls free** - corruption with no
//!    benign cause either, and the state that *becomes* the first one at the
//!    next allocation;
//! 3. **a page the free map calls allocated that no tree reaches** - a leak,
//!    which the pinned SQLite reports as `Page N: never used`.
//!
//! **The third was held back until task-2065, because the engine produced that
//! state itself from two ordinary statements** (task-2052, measured over
//! nineteen workloads, each read live and again after a checkpoint and a
//! reopen). Both are closed now, and each in the place the leak was:
//!
//! - **a rolled-back `CREATE TABLE` or `CREATE INDEX` kept its tree's root
//!   page.** The undo is row-level - `undo_to_floor` replays before-images
//!   through `tree.put` and `tree.delete` - so nothing gave the allocation
//!   back. It was invisible on the connection that did it, because the rollback
//!   leaves the tree's handle in `schema.trees` and this walk therefore still
//!   reached the page; it appeared after a reopen, when the schema is rebuilt
//!   from the catalog and nothing names that tree. `Writing::built` records
//!   every tree an open transaction builds and `undo_to_floor` releases the
//!   ones it is abandoning, which is a correction to the in-memory free map and
//!   nothing else: an allocation a transaction never committed is one recovery
//!   never replays.
//! - **`DROP TABLE` kept every page the table's out-of-line values sat on.**
//!   `release_tree` recorded the interior pages and the leaves, and
//!   `paged::free_extent` is reached only from the tree's own write paths - a
//!   row deleted, a value replaced, two leaves merged - so a tree released
//!   whole never reached it. `DELETE FROM t` before the drop gave the space
//!   back, which is why a drop of a table of small values was always sound.
//!   The pending-free list now carries a value as the reference its leaf held
//!   as well as a page number, and `flush_pending_frees` calls `free_extent`
//!   for those at the commit - so a page holding values from several trees is
//!   given back when its last slot goes rather than because this tree used it.
//!
//! **Both fixes change *when* the engine hands a page back, which is what
//! task-2043 got subtly wrong**: it freed a dropped tree's pages at the
//! statement rather than at the commit, a later `CREATE TABLE` in the same
//! transaction was given one of them, and the rollback left a catalog row
//! naming a page another table then held. Three rows were lost durably and
//! `PRAGMA integrity_check` answered `ok` about the file, which is why this
//! module exists at all. Neither fix moves that boundary: a drop's frees still
//! wait for the commit, and a build's pages are released only on the path where
//! there is going to be no commit.
//!
//! [`crate::ImportedDatabase::report_leaked_pages`] is the third state on its
//! own, kept as a public entry point because a leak is now the one state of the
//! three that no SQL statement produces - so the only way to show the arm one
//! is to damage a file on purpose, and an arm nothing has been run against is
//! an arm nobody has checked.

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
    /// Reports the first page two things both hold, that a tree holds and the
    /// free map has handed back, or that the free map holds and no tree
    /// reaches.
    ///
    /// What `PRAGMA integrity_check` runs. Run per file, because a page number
    /// means nothing without one: `main` and an attached database both have a
    /// page 7.
    ///
    /// **The third state joined the pragma in task-2065**, once the two leaks
    /// the engine produced itself were closed. Until then this passed `false`,
    /// because a pragma that answered `Page N: never used` after every
    /// `DROP TABLE` of a table holding large values would be calling a sound
    /// file damaged.
    pub(crate) fn check_page_ownership(&self) -> DbResult<()> {
        self.walk_every_page()
    }

    /// Reports the first page the free map calls allocated that no tree
    /// reaches.
    ///
    /// The same answer `PRAGMA integrity_check` now gives, reachable on its
    /// own. It stays public and tested because a leak is the one state of the
    /// three that no SQL statement produces any more, so the only way to show
    /// this arm one is to take a page out of the free map on purpose - and an
    /// arm nothing has been run against is an arm nobody has checked.
    pub fn report_leaked_pages(&self) -> DbResult<()> {
        self.walk_every_page()
    }

    /// Walks every file's pages and reports the first thing that is wrong.
    ///
    /// **It took a `leaks` flag until task-2065**, because the pragma wanted
    /// two of the three states and the public arm wanted all three. Both want
    /// all three now, so the flag went rather than staying as a knob nothing
    /// turns.
    fn walk_every_page(&self) -> DbResult<()> {
        for (at, handles) in self.trees_by_schema() {
            let file = self
                .schema_file(at)
                .ok_or_else(|| refusal("a tree names a database that is not attached"))?;
            let names = self.name_every_tree(&handles);
            let held = self.hold_every_page(file, &handles, &names)?;
            compare_against_the_free_map(file, &held, &names, self.schema.skipped.is_empty())?;
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
/// @param complete - whether every object in the catalog was attached
fn compare_against_the_free_map(
    file: &Database,
    held: &BTreeMap<PageId, Holder>,
    names: &BTreeMap<u32, String>,
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
            (None, true) if complete => return Err(damaged(format!("Page {number}: never used"))),
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
