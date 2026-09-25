//! The configuration matrix every application-shaped story runs across.
//!
//! Invariant: **a story is written once and run at every arm, and the arm's
//! name is in the test's own name, so a failure says which configuration
//! produced it before anybody opens the file.**
//!
//! ## Why a matrix exists at all
//!
//! Because the escape that prompted it was a configuration nobody varied.
//! Every test in this repository, every published performance ratio and every
//! row of the 416-case feature comparison ran at one page size, 32,768 bytes,
//! and SQLite's own default is 4,096. task-2033 is what that costs: 200 FTS5
//! documents at a 4,096 byte page refuse with `SQLITE_CORRUPT` on row 42, and
//! nothing in a suite of 3,169 tests could see it, because none of them built
//! a file with pages that small.
//!
//! The same shape produced `2fa10c4`: a 64 frame buffer pool evicted dirty
//! uncommitted pages, and the two tests that would have caught it skipped
//! because a fixture was missing. A pool that small is what an embedded caller
//! on a small device asks for and what no story here asked for.
//!
//! So the arms below are not a sweep of the option space. Each one is a
//! configuration that a real caller picks and that a defect has already hidden
//! behind.
//!
//! ## What an arm is not
//!
//! It is not a scale. The number of rows a story writes is [`Scale`], read from
//! `INILLUCENT_SCENARIO`, and it is a separate axis: the `e2e` tier runs every
//! arm at [`Scale::Quick`] and the `nightly` tier runs the long forms at
//! [`Scale::Full`]. An arm that only ran under an environment variable would be
//! a test that reports success having run nothing, which is rule 1.2 of
//! `tests/inillucent-testing-tdd.md`.

use std::path::{Path, PathBuf};

use inillucent_base::DbResult;
use inillucent_engine::connect::Database;

/// The default page size the engine builds at, and what every published number
/// in this repository was measured on.
pub const DEFAULT_PAGE_SIZE: u32 = 32_768;

/// SQLite's default page size, and the one task-2033 fails at.
pub const SQLITE_PAGE_SIZE: u32 = 4_096;

/// The default buffer pool size, in frames.
pub const DEFAULT_FRAMES: u32 = 4_096;

/// A pool small enough that eviction happens during an ordinary story.
///
/// 64 frames is the floor `ImportedDatabase::create_on` clamps to, and the size
/// `2fa10c4`'s dirty-page eviction defect needed to appear.
pub const SMALL_POOL_FRAMES: u32 = 64;

/// How a connection journals a transaction it has to be able to undo.
///
/// The engine takes this as a `PRAGMA` after the file is open rather than as a
/// constructor option, which is the answer to the TDD's open question: it is
/// what `crates/inillucent-compat/tests/durability/durability.rs::set_journal` does, and
/// doing it a second way here would be a second thing to keep true.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Journal {
    /// Whatever the connection opens with; no `PRAGMA` is issued.
    ///
    /// Deliberately not spelled `delete`. `PRAGMA journal_mode = delete`
    /// matches the default, returns without doing anything, and - as
    /// `durability.rs` records - that made the default mode the *least*
    /// exercised of the three, because the other two run two checkpoints on
    /// their way in and so crash inside a checkpoint by accident.
    Untouched,
    /// `PRAGMA journal_mode = truncate`: the commit point is the truncation.
    Truncate,
    /// `PRAGMA journal_mode = persist`: the commit point is the header write.
    Persist,
}

impl Journal {
    /// The `PRAGMA` this mode is applied with, or nothing for
    /// [`Journal::Untouched`].
    pub fn pragma(self) -> Option<&'static str> {
        match self {
            Journal::Untouched => None,
            Journal::Truncate => Some("PRAGMA journal_mode = truncate"),
            Journal::Persist => Some("PRAGMA journal_mode = persist"),
        }
    }
}

/// One configuration a scenario runs under. `name` appears in every failure
/// message and in the test's own name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Arm {
    /// The arm's name, which the expanded test is called by with hyphens folded
    /// to underscores.
    pub name: &'static str,
    /// The page size the file is built at.
    pub page_size: u32,
    /// How many frames the buffer pool holds.
    pub frames: u32,
    /// How the connection journals an undoable transaction.
    pub journal: Journal,
    /// `PRAGMA busy_timeout`, in milliseconds. Zero leaves it alone.
    pub busy_timeout_ms: u32,
}

/// Which list of arms a caller wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The three arms a long-form campaign runs in the `e2e` tier.
    Quick,
    /// All six, which the long forms in the `nightly` tier run.
    Full,
}

/// What every published number runs at: 32,768 byte pages, the default pool.
pub const fn default_arm() -> Arm {
    Arm {
        name: "default",
        page_size: DEFAULT_PAGE_SIZE,
        frames: DEFAULT_FRAMES,
        journal: Journal::Untouched,
        busy_timeout_ms: 0,
    }
}

/// SQLite's own default page size, and where task-2033 lives.
pub const fn sqlite_page_arm() -> Arm {
    Arm {
        name: "sqlite-page",
        page_size: SQLITE_PAGE_SIZE,
        frames: DEFAULT_FRAMES,
        journal: Journal::Untouched,
        busy_timeout_ms: 0,
    }
}

/// A 64 frame pool at 4,096 bytes, where `2fa10c4` evicted dirty pages.
pub const fn small_pool_arm() -> Arm {
    Arm {
        name: "small-pool",
        page_size: SQLITE_PAGE_SIZE,
        frames: SMALL_POOL_FRAMES,
        journal: Journal::Untouched,
        busy_timeout_ms: 0,
    }
}

/// TRUNCATE journalling at the default page size (task-1911).
pub const fn truncate_journal_arm() -> Arm {
    Arm {
        name: "truncate-journal",
        page_size: DEFAULT_PAGE_SIZE,
        frames: DEFAULT_FRAMES,
        journal: Journal::Truncate,
        busy_timeout_ms: 0,
    }
}

/// PERSIST journalling at SQLite's page size (task-1911).
pub const fn persist_journal_arm() -> Arm {
    Arm {
        name: "persist-journal",
        page_size: SQLITE_PAGE_SIZE,
        frames: DEFAULT_FRAMES,
        journal: Journal::Persist,
        busy_timeout_ms: 0,
    }
}

/// A connection that waits for a busy file rather than refusing at once.
pub const fn waiting_arm() -> Arm {
    Arm {
        name: "waiting",
        page_size: SQLITE_PAGE_SIZE,
        frames: DEFAULT_FRAMES,
        journal: Journal::Untouched,
        busy_timeout_ms: 2_000,
    }
}

/// The arms a campaign runs. Order is cheapest first so a failure reports early.
///
/// @param kind - `Quick` for the three the `e2e` tier runs, `Full` for all six
pub fn arms(kind: Kind) -> Vec<Arm> {
    let quick = vec![default_arm(), sqlite_page_arm(), small_pool_arm()];
    match kind {
        Kind::Quick => quick,
        Kind::Full => {
            let mut all = quick;
            all.push(truncate_journal_arm());
            all.push(persist_journal_arm());
            all.push(waiting_arm());
            all
        }
    }
}

/// How much work a scenario does, which is a different axis from the arm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scale {
    /// The `e2e` form: small enough that the tier stays under fifteen seconds.
    Quick,
    /// The `nightly` form, selected by `INILLUCENT_SCENARIO=full`.
    Full,
}

impl Scale {
    /// Reads the scale out of `INILLUCENT_SCENARIO`.
    ///
    /// Anything other than `full` is [`Scale::Quick`], including the variable
    /// being absent, because the quick form is the one that always runs. A
    /// scenario never *skips* on this: it does less work, and still asserts
    /// every value it asserts at the long form.
    pub fn from_env() -> Scale {
        match std::env::var("INILLUCENT_SCENARIO").as_deref() {
            Ok("full") => Scale::Full,
            _ => Scale::Quick,
        }
    }

    /// Picks between two sizes by scale.
    ///
    /// @param quick - the `e2e` number
    /// @param full - the `nightly` number
    pub fn pick(self, quick: usize, full: usize) -> usize {
        match self {
            Scale::Quick => quick,
            Scale::Full => full,
        }
    }
}

impl Arm {
    /// The arm's name with hyphens folded to underscores, which is what the
    /// expanded test is called.
    pub fn test_name(&self) -> String {
        self.name.replace('-', "_")
    }

    /// Opens a database at this arm's geometry, creating it if it is not there.
    ///
    /// **The page size is carried into the open rather than applied after it.**
    /// `PRAGMA page_size` in this engine reports the geometry and does not set
    /// it, so a file built at the default and then asked to be 4,096 bytes is
    /// still a 32,768 byte file - which is exactly the way a matrix can look
    /// like it is varying something while varying nothing.
    ///
    /// @param path - the database file
    pub fn open(&self, path: &Path) -> DbResult<Database> {
        let database = Database::open_at(path, self.page_size as usize, self.frames as usize)?;
        self.apply(&database)?;
        Ok(database)
    }

    /// Applies the journal mode and the busy timeout to an open database.
    ///
    /// Separate from [`Arm::open`] because a story that reopens a file part way
    /// through - and every story does, rule 1.4 - has to put the connection
    /// back into the arm's configuration afterwards.
    ///
    /// @param database - the open database
    pub fn apply(&self, database: &Database) -> DbResult<()> {
        let connection = database.session();
        if let Some(pragma) = self.journal.pragma() {
            connection.execute(pragma)?;
        }
        if self.busy_timeout_ms > 0 {
            connection.execute(&format!("PRAGMA busy_timeout = {}", self.busy_timeout_ms))?;
        }
        Ok(())
    }

    /// The scratch directory for one story at this arm, emptied first.
    ///
    /// **The arm's name is in the path**, so two arms of one story never share
    /// a file: a `small-pool` run that left a half-written log where the
    /// `default` run expected a fresh file would be a failure in whichever arm
    /// happened to run second, which is not a thing anybody can debug.
    ///
    /// @param base - `CARGO_TARGET_TMPDIR` of the test crate
    /// @param story - the story's name
    pub fn area(&self, base: &str, story: &str) -> PathBuf {
        let area = PathBuf::from(base)
            .join("scenarios")
            .join(story)
            .join(self.name);
        let _ = std::fs::remove_dir_all(&area);
        let _ = std::fs::create_dir_all(&area);
        area
    }
}

/// Runs one story at one arm. The body of every test [`scenario`] expands.
///
/// @param story - the story's name, which names the scratch directory
/// @param arm - the configuration to run at
/// @param base - `CARGO_TARGET_TMPDIR` of the test crate
/// @param body - the story
pub fn run(story: &str, arm: Arm, base: &str, body: fn(&Arm, &Path)) {
    let area = arm.area(base, story);
    body(&arm, &area);
}

/// Expands one test per arm for a story written once.
///
/// The story is `fn(&Arm, &Path)`: the arm it is running at, and a scratch
/// directory of its own. `scenario!(ingest_and_search, ingest_and_search)`
/// produces `ingest_and_search::default`, `::sqlite_page`, `::small_pool`,
/// `::truncate_journal`, `::persist_journal` and `::waiting`, each its own test
/// with its own verdict.
///
/// **Every arm is expanded, not just the quick three.** A test that exists only
/// when an environment variable is set is a test that reports success having
/// run nothing, and the arms do not multiply a story's work - they change the
/// geometry it runs over. The axis that does multiply work is [`Scale`], which
/// a story reads at run time.
#[macro_export]
macro_rules! scenario {
    ($name:ident, $story:path) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            /// The story at 32,768 byte pages: what every published number runs at.
            #[test]
            fn default() {
                $crate::matrix::run(
                    stringify!($name),
                    $crate::matrix::default_arm(),
                    env!("CARGO_TARGET_TMPDIR"),
                    $story,
                )
            }

            /// The story at SQLite's own 4,096 byte page size (task-2033).
            #[test]
            fn sqlite_page() {
                $crate::matrix::run(
                    stringify!($name),
                    $crate::matrix::sqlite_page_arm(),
                    env!("CARGO_TARGET_TMPDIR"),
                    $story,
                )
            }

            /// The story with a 64 frame pool, where eviction happens.
            #[test]
            fn small_pool() {
                $crate::matrix::run(
                    stringify!($name),
                    $crate::matrix::small_pool_arm(),
                    env!("CARGO_TARGET_TMPDIR"),
                    $story,
                )
            }

            /// The story under TRUNCATE journalling.
            #[test]
            fn truncate_journal() {
                $crate::matrix::run(
                    stringify!($name),
                    $crate::matrix::truncate_journal_arm(),
                    env!("CARGO_TARGET_TMPDIR"),
                    $story,
                )
            }

            /// The story under PERSIST journalling.
            #[test]
            fn persist_journal() {
                $crate::matrix::run(
                    stringify!($name),
                    $crate::matrix::persist_journal_arm(),
                    env!("CARGO_TARGET_TMPDIR"),
                    $story,
                )
            }

            /// The story on a connection that waits for a busy file.
            #[test]
            fn waiting() {
                $crate::matrix::run(
                    stringify!($name),
                    $crate::matrix::waiting_arm(),
                    env!("CARGO_TARGET_TMPDIR"),
                    $story,
                )
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Quick` is the first three of `Full`, in the same order.
    #[test]
    fn quick_is_the_first_three_of_full() {
        let quick = arms(Kind::Quick);
        let full = arms(Kind::Full);
        assert_eq!(quick.len(), 3, "{quick:?}");
        assert_eq!(full.len(), 6, "{full:?}");
        assert_eq!(
            &full[..3],
            &quick[..],
            "the quick arms are not full's prefix"
        );
    }

    /// Every arm has a distinct name, because the name is the scratch directory.
    #[test]
    fn every_arm_is_named_once() {
        let mut names: Vec<&str> = arms(Kind::Full).iter().map(|arm| arm.name).collect();
        names.sort_unstable();
        let mut unique = names.clone();
        unique.dedup();
        assert_eq!(names, unique, "two arms share a name: {names:?}");
    }

    /// The matrix varies what it says it varies.
    ///
    /// Four arms run at SQLite's page size and one of those has a small pool;
    /// one runs TRUNCATE and one PERSIST; one waits. A matrix whose arms all
    /// carried the same numbers would pass every scenario six times and mean
    /// nothing, which is what the 32,768-byte-only suite was already doing
    /// once.
    #[test]
    fn the_arms_differ_in_the_things_the_escapes_needed() {
        let full = arms(Kind::Full);
        let at_sqlite_page = full
            .iter()
            .filter(|arm| arm.page_size == SQLITE_PAGE_SIZE)
            .count();
        assert_eq!(at_sqlite_page, 4, "{full:?}");
        assert_eq!(
            full.iter()
                .filter(|arm| arm.frames == SMALL_POOL_FRAMES)
                .count(),
            1,
            "{full:?}"
        );
        assert_eq!(
            full.iter()
                .filter(|arm| arm.journal == Journal::Truncate)
                .count(),
            1,
            "{full:?}"
        );
        assert_eq!(
            full.iter()
                .filter(|arm| arm.journal == Journal::Persist)
                .count(),
            1,
            "{full:?}"
        );
        assert_eq!(
            full.iter().filter(|arm| arm.busy_timeout_ms > 0).count(),
            1,
            "{full:?}"
        );
    }

    /// A test name is the arm's name with hyphens folded, which is what the
    /// guard in `scenarios.rs` matches against.
    #[test]
    fn a_test_name_folds_the_hyphen() {
        assert_eq!(sqlite_page_arm().test_name(), "sqlite_page");
        assert_eq!(default_arm().test_name(), "default");
        assert_eq!(truncate_journal_arm().test_name(), "truncate_journal");
    }

    /// The scale is `Quick` unless the variable says `full`.
    #[test]
    fn the_scale_picks_the_quick_number_by_default() {
        assert_eq!(Scale::Quick.pick(3_000, 100_000), 3_000);
        assert_eq!(Scale::Full.pick(3_000, 100_000), 100_000);
    }
}
