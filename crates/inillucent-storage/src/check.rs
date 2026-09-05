//! Raw quick and integrity checks, read-only and independent of the SQL layer.
//!
//! Invariant: a check never stops at the first problem, and it never trusts a
//! page it has already refused. It collects findings, bounds every walk by the
//! database's own page count, and returns a report - because the point of an
//! integrity check is to describe a damaged file, and a function that returns
//! the first error tells you nothing about how bad it is.
//!
//! Two levels, matching SQLite's own two pragmas:
//!
//! - the **quick** check validates every reachable page's structure and every
//!   cell's arithmetic, and confirms that keys are in order within a page and
//!   across a page boundary. It is the check a reader would have to pass
//!   anyway, run eagerly over the whole file;
//! - the **integrity** check adds the whole-file accounting: every page is
//!   used exactly once, the freelist is the length the header claims, cell
//!   content and freeblocks tile each page's content area exactly, and no page
//!   is both in a tree and on the freelist.
//!
//! Neither reads the SQL in `sqlite_schema` beyond the root pages, so a file
//! whose CREATE statements are unparseable can still be checked.

use std::collections::BTreeMap;

use inillucent_base::bytes;
use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_value::record::{KeyInfo, RecordRef};
use inillucent_value::{record, TextEncoding};

use crate::btree::{BTreePage, PageKind};
use crate::header::VacuumMode;
use crate::overflow;
use crate::pager::Pager;
use crate::ptrmap;
use crate::schema;

/// How thorough a check to run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckLevel {
    /// Validate every reachable page and the order of the keys on it.
    Quick,
    /// Also account for every page in the file exactly once.
    Integrity,
}

/// How many problems a check reports before it stops describing them.
///
/// SQLite's own `PRAGMA integrity_check` takes a limit and defaults to 100,
/// for the reason this one exists: a header field is four bytes and can claim
/// four billion pages, and a check that described every one of them would
/// allocate until the process died. The check keeps counting after the cap; it
/// just stops writing sentences about it.
pub const DEFAULT_PROBLEM_LIMIT: usize = 100;

/// What a check found.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CheckReport {
    /// The problems found, up to the report's limit.
    pub problems: Vec<String>,
    /// How many problems were found in total, including those not described.
    pub problem_count: u64,
    /// The most problems this report will describe.
    pub problem_limit: usize,
    /// How many pages were reached from a B-tree root.
    pub tree_pages: u64,
    /// How many pages the freelist holds.
    pub freelist_pages: u64,
    /// How many pages are overflow pages.
    pub overflow_pages: u64,
    /// How many pages are pointer-map pages.
    pub pointer_map_pages: u64,
    /// How many entries were visited.
    pub entries: u64,
    /// How many B-tree roots were checked.
    pub roots: u64,
}

impl CheckReport {
    /// Builds an empty report with the default problem limit.
    pub fn new() -> CheckReport {
        CheckReport {
            problem_limit: DEFAULT_PROBLEM_LIMIT,
            ..CheckReport::default()
        }
    }

    /// Reports whether the database passed.
    pub fn is_ok(&self) -> bool {
        self.problem_count == 0
    }

    /// Reports whether the report stopped describing problems.
    pub fn is_truncated(&self) -> bool {
        self.problem_count > self.problems.len() as u64
    }

    /// Returns the text SQLite's own pragma would print.
    pub fn as_pragma_output(&self) -> Vec<String> {
        if self.problems.is_empty() {
            vec!["ok".to_string()]
        } else {
            self.problems.clone()
        }
    }

    /// Records a problem, describing it only while there is room.
    fn report(&mut self, problem: impl Into<String>) {
        self.problem_count = self.problem_count.saturating_add(1);
        let limit = if self.problem_limit == 0 {
            DEFAULT_PROBLEM_LIMIT
        } else {
            self.problem_limit
        };
        if self.problems.len() < limit {
            self.problems.push(problem.into());
        }
    }

    /// Reports whether the check should stop looking.
    fn is_full(&self) -> bool {
        let limit = if self.problem_limit == 0 {
            DEFAULT_PROBLEM_LIMIT
        } else {
            self.problem_limit
        };
        self.problems.len() >= limit
    }
}

/// What a page is being used for, so a second use can be named.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageUse {
    /// A B-tree page belonging to the tree rooted at this page.
    Tree(u32),
    /// A page in an overflow chain.
    Overflow,
    /// A freelist trunk page.
    FreelistTrunk,
    /// A freelist leaf page.
    FreelistLeaf,
    /// A pointer-map page.
    PointerMap,
    /// Page 1, which is the header and the schema root at once.
    Header,
    /// The page holding the byte-range lock, which belongs to nothing.
    LockByte,
}

impl PageUse {
    /// Returns a name for a message.
    fn describe(self) -> String {
        match self {
            PageUse::Tree(root) => format!("the tree rooted at page {root}"),
            PageUse::Overflow => "an overflow chain".to_string(),
            PageUse::FreelistTrunk => "a freelist trunk".to_string(),
            PageUse::FreelistLeaf => "the freelist".to_string(),
            PageUse::PointerMap => "a pointer map".to_string(),
            PageUse::Header => "page 1".to_string(),
            PageUse::LockByte => "the lock byte".to_string(),
        }
    }
}

/// Runs a check over a whole database.
///
/// Index key order is *not* checked here, and cannot be: the order an index is
/// in is decided by the collation and direction its `CREATE INDEX` declares,
/// and that declaration is SQL text the storage layer does not parse. A check
/// that assumed BINARY ascending would report every `DESC` and every
/// `COLLATE NOCASE` index as corrupt. Use [`check_database_with_keys`] when
/// the caller knows the declarations - the catalog does, and the compatibility
/// suite does for its own corpus.
pub fn check_database(pager: &mut Pager, level: CheckLevel) -> DbResult<CheckReport> {
    check_database_with_keys(pager, level, &BTreeMap::new())
}

/// What a check should look at.
///
/// The default is what `PRAGMA integrity_check` does: read `sqlite_schema`,
/// walk every root it names, and account for every page. The other two fields
/// exist because storage has callers above it that know things the schema does
/// not - a caller holding a tree that is not in the schema yet, and a caller
/// that knows how an index's keys are declared to sort.
#[derive(Clone, Debug)]
pub struct CheckOptions {
    /// How much of the check to run.
    pub level: CheckLevel,
    /// Whether to read `sqlite_schema` for the roots to walk.
    pub schema_roots: bool,
    /// Roots to walk in addition to the ones the schema names.
    pub extra_roots: Vec<PageId>,
    /// How each index's keys are declared to sort, by root page.
    pub index_keys: BTreeMap<u32, KeyInfo>,
}

impl Default for CheckOptions {
    /// The options `PRAGMA integrity_check` runs with.
    fn default() -> CheckOptions {
        CheckOptions::integrity()
    }
}

impl CheckOptions {
    /// The options `PRAGMA integrity_check` runs with.
    pub fn integrity() -> CheckOptions {
        CheckOptions {
            level: CheckLevel::Integrity,
            schema_roots: true,
            extra_roots: Vec::new(),
            index_keys: BTreeMap::new(),
        }
    }

    /// A check of trees the caller names, without reading the schema.
    ///
    /// Storage has no catalog, so a test or a layer above that has just built a
    /// tree cannot put it in `sqlite_schema` to have it checked. Naming the
    /// roots directly is how the mutation suite checks what it built.
    pub fn roots(roots: Vec<PageId>) -> CheckOptions {
        CheckOptions {
            level: CheckLevel::Integrity,
            schema_roots: false,
            extra_roots: roots,
            index_keys: BTreeMap::new(),
        }
    }

    /// Adds an index's declared key ordering.
    pub fn with_key(mut self, root: PageId, key: KeyInfo) -> CheckOptions {
        self.index_keys.insert(root.get(), key);
        self
    }
}

/// Runs a check, using the caller's knowledge of each index's key order.
///
/// `index_keys` maps a B-tree's root page to the ordering its declaration
/// gives it. A root with no entry has its structure checked but not its order.
pub fn check_database_with_keys(
    pager: &mut Pager,
    level: CheckLevel,
    index_keys: &BTreeMap<u32, KeyInfo>,
) -> DbResult<CheckReport> {
    check_database_with_options(
        pager,
        &CheckOptions {
            level,
            schema_roots: true,
            extra_roots: Vec::new(),
            index_keys: index_keys.clone(),
        },
    )
}

/// Runs a check over exactly the trees the options name.
pub fn check_database_with_options(
    pager: &mut Pager,
    options: &CheckOptions,
) -> DbResult<CheckReport> {
    let level = options.level;
    let index_keys = &options.index_keys;
    let mut report = CheckReport::new();
    let mut uses: BTreeMap<u32, PageUse> = BTreeMap::new();
    let mut expected: BTreeMap<u32, ptrmap::Entry> = BTreeMap::new();
    uses.insert(1, PageUse::Header);

    let mut roots = Vec::new();
    if options.schema_roots {
        let objects = match schema::load_schema(pager) {
            Ok(objects) => objects,
            Err(error) => {
                report.report(format!("the schema could not be read: {error}"));
                Vec::new()
            }
        };
        // Page 1 is the schema's own root and is checked like any other tree.
        roots.push(PageId::from_persisted(schema::SCHEMA_ROOT)?);
        roots.extend(schema::root_pages(&objects));
    }
    for root in &options.extra_roots {
        if !roots.contains(root) {
            roots.push(*root);
        }
    }

    let limits = Limits::default();
    let encoding = pager.text_encoding();
    for root in roots {
        report.roots = report.roots.saturating_add(1);
        let declared = index_keys.get(&root.get()).cloned();
        let key = declared.clone().unwrap_or_default();
        let check_order = declared.is_some();
        if root.get() != 1 {
            expected.insert(root.get(), ptrmap::Entry::root());
        }
        if let Err(error) = check_tree(
            pager,
            root,
            root,
            &mut report,
            &mut uses,
            &mut expected,
            &limits,
            encoding,
            &key,
            check_order,
            level,
        ) {
            report.report(format!(
                "the tree rooted at page {} could not be walked: {error}",
                root.get()
            ));
        }
    }

    if let Err(error) = check_freelist(pager, &mut report, &mut uses, &mut expected) {
        report.report(format!("the freelist could not be walked: {error}"));
    }

    if level == CheckLevel::Integrity {
        account_for_every_page(pager, &mut report, &mut uses)?;
        verify_pointer_maps(pager, &mut report, &expected)?;
    }

    for use_kind in uses.values() {
        match use_kind {
            PageUse::Overflow => report.overflow_pages = report.overflow_pages.saturating_add(1),
            PageUse::FreelistTrunk | PageUse::FreelistLeaf => {
                report.freelist_pages = report.freelist_pages.saturating_add(1)
            }
            PageUse::PointerMap => {
                report.pointer_map_pages = report.pointer_map_pages.saturating_add(1)
            }
            PageUse::Tree(_) | PageUse::Header => {
                report.tree_pages = report.tree_pages.saturating_add(1)
            }
            PageUse::LockByte => {}
        }
    }
    Ok(report)
}

/// Walks one B-tree, validating every page and the order of its keys.
#[allow(clippy::too_many_arguments)]
fn check_tree(
    pager: &mut Pager,
    root: PageId,
    page_id: PageId,
    report: &mut CheckReport,
    uses: &mut BTreeMap<u32, PageUse>,
    expected: &mut BTreeMap<u32, ptrmap::Entry>,
    limits: &Limits,
    encoding: TextEncoding,
    key: &KeyInfo,
    check_order: bool,
    level: CheckLevel,
) -> DbResult<Option<Vec<u8>>> {
    claim(uses, page_id, PageUse::Tree(root.get()), report);
    let pin = pager.get_page(page_id)?;
    let usable = pager.usable_size()?;
    let layout = match pin.layout(usable) {
        Ok(layout) => layout,
        Err(error) => {
            report.report(format!("page {} is malformed: {error}", page_id.get()));
            return Ok(None);
        }
    };
    let page = BTreePage::new(pin.bytes(), &layout);

    if level == CheckLevel::Integrity {
        if let Err(error) = page.check_layout() {
            report.report(format!(
                "page {} does not tile its content area: {error}",
                page_id.get()
            ));
        }
    }

    let mut last_key: Option<Vec<u8>> = None;
    let mut last_rowid: Option<i64> = None;
    let is_table = page.kind().is_table();

    for index in 0..page.cell_count() {
        let cell = match page.cell(index) {
            Ok(cell) => cell,
            Err(error) => {
                report.report(format!(
                    "cell {index} on page {} is malformed: {error}",
                    page_id.get()
                ));
                continue;
            }
        };

        if let Some(child) = cell.left_child {
            expected.insert(child.get(), ptrmap::Entry::child_of(page_id));
            let child_key = check_tree(
                pager,
                root,
                child,
                report,
                uses,
                expected,
                limits,
                encoding,
                key,
                check_order,
                level,
            )?;
            if !is_table && check_order {
                if let (Some(previous), Some(child_last)) = (last_key.as_ref(), child_key.as_ref())
                {
                    if compare_keys(child_last, previous, encoding, key, limits)
                        .is_some_and(|ordering| ordering == std::cmp::Ordering::Less)
                    {
                        report.report(format!(
                            "page {} has a child subtree whose last key precedes an earlier one",
                            page_id.get()
                        ));
                    }
                }
            }
        }

        if is_table {
            if let Some(rowid) = cell.rowid {
                if let Some(previous) = last_rowid {
                    if rowid <= previous {
                        report.report(format!(
                            "page {} has rowid {rowid} after {previous}, which is out of order",
                            page_id.get()
                        ));
                    }
                }
                last_rowid = Some(rowid);
            }
        }

        if cell.split.overflows {
            match overflow::chain_pages(pager, cell.split.total, cell.split.local, cell.overflow) {
                Ok(pages) => {
                    let mut previous: Option<PageId> = None;
                    for overflow_page in pages {
                        claim(uses, overflow_page, PageUse::Overflow, report);
                        let entry = match previous {
                            Some(before) => ptrmap::Entry::overflow_next(before),
                            None => ptrmap::Entry::overflow_head(page_id),
                        };
                        expected.insert(overflow_page.get(), entry);
                        previous = Some(overflow_page);
                    }
                }
                Err(error) => report.report(format!(
                    "cell {index} on page {} has a bad overflow chain: {error}",
                    page_id.get()
                )),
            }
        }

        if page.kind() != PageKind::InteriorTable {
            let payload = match overflow::read_payload(
                pager,
                cell.local_payload,
                cell.split.total,
                cell.overflow,
                limits,
            ) {
                Ok(payload) => payload,
                Err(error) => {
                    report.report(format!(
                        "cell {index} on page {} could not be read: {error}",
                        page_id.get()
                    ));
                    continue;
                }
            };
            if let Err(error) = RecordRef::parse_with_limits(&payload, encoding, limits) {
                report.report(format!(
                    "cell {index} on page {} holds a malformed record: {error}",
                    page_id.get()
                ));
            }
            report.entries = report.entries.saturating_add(1);
            if !is_table {
                if check_order {
                    if let Some(previous) = last_key.as_ref() {
                        if compare_keys(&payload, previous, encoding, key, limits)
                            .is_some_and(|ordering| ordering != std::cmp::Ordering::Greater)
                        {
                            report.report(format!(
                                "page {} has index keys out of order at cell {index}",
                                page_id.get()
                            ));
                        }
                    }
                }
                last_key = Some(payload);
            }
        } else {
            report.entries = report.entries.saturating_add(1);
        }
    }

    if let Some(right) = page.right_child() {
        expected.insert(right.get(), ptrmap::Entry::child_of(page_id));
        let child_key = check_tree(
            pager,
            root,
            right,
            report,
            uses,
            expected,
            limits,
            encoding,
            key,
            check_order,
            level,
        )?;
        if !is_table && child_key.is_some() {
            last_key = child_key;
        }
        let _ = &last_key;
    }

    Ok(last_key)
}

/// Compares two encoded index keys, returning `None` when either is malformed.
fn compare_keys(
    left: &[u8],
    right: &[u8],
    encoding: TextEncoding,
    key: &KeyInfo,
    limits: &Limits,
) -> Option<std::cmp::Ordering> {
    let left = RecordRef::parse_with_limits(left, encoding, limits).ok()?;
    let right = RecordRef::parse_with_limits(right, encoding, limits).ok()?;
    record::compare_records(&left, &right, key).ok()
}

/// Walks the freelist, checking its shape and its length.
fn check_freelist(
    pager: &mut Pager,
    report: &mut CheckReport,
    uses: &mut BTreeMap<u32, PageUse>,
    expected: &mut BTreeMap<u32, ptrmap::Entry>,
) -> DbResult<()> {
    let head = pager.header().freelist_head;
    let claimed = pager.header().freelist_count;
    let page_count = pager.page_count();
    let usable = pager.usable_size()?;
    let mut counted = 0u32;
    let mut next = head;
    let mut trunks = 0u32;

    while next != 0 {
        trunks = trunks.saturating_add(1);
        if trunks > page_count {
            report.report("the freelist has more trunk pages than the database has pages");
            break;
        }
        let Ok(trunk_id) = PageId::from_persisted(next) else {
            report.report("the freelist points at page zero");
            break;
        };
        if trunk_id.get() > page_count {
            report.report(format!(
                "the freelist points at page {} outside a {page_count}-page database",
                trunk_id.get()
            ));
            break;
        }
        claim(uses, trunk_id, PageUse::FreelistTrunk, report);
        expected.insert(trunk_id.get(), ptrmap::Entry::free());
        counted = counted.saturating_add(1);
        let pin = pager.get_page(trunk_id)?;
        let bytes_of = pin.bytes();
        let following = bytes::read_u32(bytes_of, 0)?;
        let leaves = bytes::read_u32(bytes_of, 4)?;
        let capacity = usable.saturating_sub(8) / 4;
        if leaves > capacity {
            report.report(format!(
                "freelist trunk page {} claims {leaves} leaves, more than the {capacity} that fit",
                trunk_id.get()
            ));
            break;
        }
        for index in 0..leaves {
            let at = 8usize.saturating_add((index as usize).saturating_mul(4));
            let leaf = bytes::read_u32(bytes_of, at)?;
            let Ok(leaf_id) = PageId::from_persisted(leaf) else {
                report.report(format!(
                    "freelist trunk page {} lists page zero",
                    trunk_id.get()
                ));
                continue;
            };
            if leaf_id.get() > page_count {
                report.report(format!(
                    "freelist trunk page {} lists page {} outside the database",
                    trunk_id.get(),
                    leaf_id.get()
                ));
                continue;
            }
            claim(uses, leaf_id, PageUse::FreelistLeaf, report);
            expected.insert(leaf_id.get(), ptrmap::Entry::free());
            counted = counted.saturating_add(1);
        }
        next = following;
    }

    if counted != claimed {
        report.report(format!(
            "the header says the freelist holds {claimed} pages but {counted} were found"
        ));
    }
    Ok(())
}

/// Checks that every page in the file is accounted for exactly once.
///
/// The walk is bounded by the pages the *file* holds, not by the count the
/// header claims. A header can claim four billion pages in four bytes, and a
/// loop over that number describes a problem per page until the process runs
/// out of memory - which is what happened here before the bound was added.
fn account_for_every_page(
    pager: &mut Pager,
    report: &mut CheckReport,
    uses: &mut BTreeMap<u32, PageUse>,
) -> DbResult<()> {
    let claimed = pager.page_count();
    // A writer's pages reach the file at commit, so during a transaction the
    // database is legitimately longer than the file it lives in. Outside one,
    // a header that claims more pages than the file holds is the corruption
    // this check exists to report.
    let in_file = if pager.is_writing() {
        claimed.max(pager.pages_in_file())
    } else {
        pager.pages_in_file()
    };
    if claimed > in_file {
        report.report(format!(
            "the header claims {claimed} pages but the file holds {in_file}"
        ));
    }
    let page_count = claimed.min(in_file);
    let header = *pager.header();
    for number in 1..=page_count {
        if report.is_full() {
            break;
        }
        let Ok(page_id) = PageId::from_persisted(number) else {
            continue;
        };
        if header.vacuum_mode != VacuumMode::None && header.is_pointer_map_page(page_id)? {
            claim(uses, page_id, PageUse::PointerMap, report);
            continue;
        }
        if number == crate::alloc::lock_byte_page(header.page_size) {
            // The page holding the lock byte belongs to nothing, in every
            // database, at every page size. Only a file larger than a gigabyte
            // has one at all.
            claim(uses, page_id, PageUse::LockByte, report);
            continue;
        }
        if !uses.contains_key(&number) {
            report.report(format!(
                "page {number} is not on the freelist and is not part of any tree"
            ));
        }
    }
    for number in uses.keys() {
        if *number > page_count {
            report.report(format!(
                "page {number} is used but is outside a {page_count}-page database"
            ));
        }
    }
    Ok(())
}

/// Compares every pointer-map entry against what the traversal found.
///
/// This is the check that makes auto-vacuum safe rather than hopeful. A stale
/// reverse pointer is invisible until something moves the page it describes,
/// and then it rewrites four bytes of a page that does not point there - which
/// is a corruption with no symptom at the time and no way back afterwards.
fn verify_pointer_maps(
    pager: &mut Pager,
    report: &mut CheckReport,
    expected: &BTreeMap<u32, ptrmap::Entry>,
) -> DbResult<()> {
    if !ptrmap::is_enabled(pager) {
        return Ok(());
    }
    let page_count = pager.page_count();
    for (number, wanted) in expected {
        if report.is_full() {
            break;
        }
        if *number > page_count {
            continue;
        }
        let Ok(page) = PageId::from_persisted(*number) else {
            continue;
        };
        let Some(found) = ptrmap::get(pager, page)? else {
            continue;
        };
        if found != *wanted {
            report.report(format!(
                "page {number} is {} whose parent is page {}, but its pointer map says {} whose parent is page {}",
                wanted.describe(),
                wanted.parent,
                found.describe(),
                found.parent
            ));
        }
    }
    Ok(())
}

/// Records that a page is in use, reporting a second use of the same page.
fn claim(
    uses: &mut BTreeMap<u32, PageUse>,
    page: PageId,
    use_kind: PageUse,
    report: &mut CheckReport,
) {
    if let Some(existing) = uses.get(&page.get()) {
        if !(matches!(existing, PageUse::Header) && matches!(use_kind, PageUse::Tree(1))) {
            report.report(format!(
                "page {} is used by {} and also by {}",
                page.get(),
                existing.describe(),
                use_kind.describe()
            ));
        }
        return;
    }
    uses.insert(page.get(), use_kind);
}

/// Runs the quick check, which validates every reachable page.
pub fn quick_check(pager: &mut Pager) -> DbResult<CheckReport> {
    check_database(pager, CheckLevel::Quick)
}

/// Runs the integrity check, which also accounts for every page in the file.
pub fn integrity_check(pager: &mut Pager) -> DbResult<CheckReport> {
    check_database(pager, CheckLevel::Integrity)
}

/// Reports whether a corruption report names a page.
///
/// The fuzz corpus asserts on the *shape* of a failure rather than on its
/// wording, and this is how it does that without matching on message text.
pub fn mentions_page(report: &CheckReport, page: u32) -> bool {
    let needle = format!("page {page}");
    report
        .problems
        .iter()
        .any(|problem| problem.contains(&needle))
}
