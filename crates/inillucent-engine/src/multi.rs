//! The commit that spans two databases, and the file that decides it.
//!
//! Invariant: **a transaction over several files is decided by the existence of
//! one file.** Every participant's log carries a `Commit` record for the
//! transaction, and none of those records is the decision: each is a *vote*.
//! Beside each participant sits a marker naming a super-journal, and the
//! super-journal's deletion is the moment every participant committed. Until it
//! happens, every one of them recovers as though the commit never arrived. There
//! is no window in which one file has committed and another has not, because
//! there is nothing to observe between "the file is there" and "the file is
//! gone": a deletion is one operation.
//!
//! Reference: <https://sqlite.org/atomiccommit.html#_multi_file_commit>. The old
//! engine's `inillucent-transaction::super_journal` is the same idea against a
//! rollback journal, where the polarity is the other way round - it replays a
//! journal *while* the super-journal is there. This log is redo-only, so what
//! the marker suppresses is a replay rather than causing one, and the mechanism
//! that carries the suppression into recovery is `RecoveryStart::doubtful`
//! rather than a name in a journal header.
//!
//! ## What it costs a transaction that wrote one file
//!
//! Nothing. Not a stat, not an open, not a byte. `commit_batch` counts the
//! schemas the transaction wrote and takes the single-file path unless there are
//! two or more, which is every statement the performance gate measures.

use std::collections::BTreeSet;

use inillucent_base::error::misuse;
use inillucent_base::DbResult;

/// The suffix a super-journal's name is built with.
///
/// SQLite's, so that a directory holding one is recognisable to a person who has
/// seen SQLite's - and so that `attach.rs`'s cleanup assertion, which looks for
/// `-mj` in a leftover file's name, is checking the thing it says it is.
pub const SUPER_JOURNAL_PREFIX: &str = "-mj";

/// The suffix of the marker that sits beside each participant.
///
/// It holds the super-journal's path and the transaction number, which is
/// everything the next open of *that* file needs to know that its `Commit` was a
/// vote rather than a decision.
pub const MARKER_SUFFIX: &str = "-mjref";

/// Returns the marker path that sits beside one database file.
///
/// @param database - the database file's path
pub fn marker_path(database: &std::path::Path) -> std::path::PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(MARKER_SUFFIX);
    std::path::PathBuf::from(name)
}

/// Returns the transactions whose `Commit` records in one file are votes rather
/// than decisions.
///
/// Reads the marker beside the file, if there is one, and asks the question the
/// marker exists to answer: is the super-journal it names still there?
///
/// - **No marker** - nothing was ever in doubt; recovery proceeds unchanged, and
///   this is the answer for every database that has never been attached to.
/// - **A marker naming a super-journal that still exists** - the commit was
///   never decided, so the transaction it names must not be replayed. The marker
///   is left in place; the crash that follows this open must find the same
///   answer.
/// - **A marker naming a super-journal that is gone** - the commit point was
///   passed, so the transaction did commit and is replayed as usual. The marker
///   is removed, because the question it asked has been answered.
///
/// @param database - the database file the marker sits beside
pub fn doubtful_transactions(database: &std::path::Path) -> DbResult<BTreeSet<u64>> {
    let marker = marker_path(database);
    let Ok(text) = std::fs::read_to_string(&marker) else {
        return Ok(BTreeSet::new());
    };
    let mut doubtful = BTreeSet::new();
    let mut settled = true;
    for line in text.lines() {
        let Some((txn, path)) = line.split_once('\t') else {
            continue;
        };
        let Ok(txn) = txn.parse::<u64>() else {
            continue;
        };
        if std::path::Path::new(path).exists() {
            doubtful.insert(txn);
            settled = false;
        }
    }
    if settled {
        // The question has an answer, so the marker has done its work. A failure
        // to remove it is not a failure of this open: the next one asks the same
        // question and gets the same answer, because the answer is the
        // super-journal's absence rather than the marker's.
        let _ = std::fs::remove_file(&marker);
    }
    Ok(doubtful)
}

/// The file whose deletion commits a transaction across several databases.
///
/// **It has no destructor, deliberately.** A handle that is dropped without
/// [`SuperJournal::commit`] leaves the file where it is, which makes the
/// transaction undecided and therefore uncommitted - the safe outcome, and the
/// one every early return from the commit path gets without having to remember
/// to ask for it. [`SuperJournal::abandon`] is the *other* thing, for the case
/// where nothing has voted yet and the files are simply litter.
pub struct SuperJournal {
    /// Where it is, which is what every marker names.
    path: std::path::PathBuf,
    /// The markers written beside the participants, to be removed after.
    markers: Vec<std::path::PathBuf>,
}

impl SuperJournal {
    /// Creates a super-journal beside one of the participants and lists them all
    /// in it.
    ///
    /// **The list exists before any marker names it.** A marker naming a
    /// super-journal nobody wrote would be a marker that suppresses a replay for
    /// ever, so the order is: write the list, sync it, then point at it.
    ///
    /// The name carries the transaction number and a counter rather than
    /// randomness, because this engine is one process per file: two
    /// super-journals cannot be in flight over the same database at once, and a
    /// name that can be read is worth more here than a name that cannot collide
    /// with another process's.
    ///
    /// @param near - the database the super-journal sits beside
    /// @param txn - the transaction it decides
    /// @param participants - the database files it decides for
    pub fn create(
        near: &std::path::Path,
        txn: u64,
        participants: &[std::path::PathBuf],
    ) -> DbResult<SuperJournal> {
        let mut name = near.as_os_str().to_os_string();
        name.push(format!("{SUPER_JOURNAL_PREFIX}{txn:08x}"));
        let path = std::path::PathBuf::from(name);
        let mut listed = String::new();
        for file in participants {
            listed.push_str(&file.display().to_string());
            listed.push('\n');
        }
        write_durably(&path, listed.as_bytes())?;
        Ok(SuperJournal {
            path,
            markers: Vec::new(),
        })
    }

    /// Writes the marker that puts one participant's commit in doubt.
    ///
    /// It has to be durable before that participant's `Commit` record is, which
    /// is why this returns before the caller appends: a `Commit` that reached
    /// the disk while the marker had not would be replayed by a recovery that
    /// never learned to doubt it.
    ///
    /// @param database - the participant's database file
    /// @param txn - the transaction the marker is about
    pub fn mark(&mut self, database: &std::path::Path, txn: u64) -> DbResult<()> {
        let marker = marker_path(database);
        let line = format!("{txn}\t{}\n", self.path.display());
        write_durably(&marker, line.as_bytes())?;
        self.markers.push(marker);
        Ok(())
    }

    /// Deletes the super-journal, which is the commit.
    ///
    /// **This one deletion decides every participant.** Before it, every one of
    /// them recovers without the transaction; after it, every one of them
    /// recovers with it. The markers are tidied afterwards and their removal
    /// decides nothing - a crash between the two leaves markers whose
    /// super-journal is gone, which the next open reads as "committed" and
    /// clears.
    pub fn commit(self) -> DbResult<()> {
        std::fs::remove_file(&self.path).map_err(|error| {
            misuse(format!(
                "cannot commit across databases: {} could not be removed: {error}",
                self.path.display()
            ))
        })?;
        for marker in &self.markers {
            let _ = std::fs::remove_file(marker);
        }
        Ok(())
    }

    /// Removes the super-journal and every marker, for a transaction that never
    /// voted.
    ///
    /// **Only before the first `Commit` record is written**, which is the one
    /// window in which these files decide nothing: no participant has voted, so
    /// there is nothing for the super-journal's presence to suppress and it is
    /// litter rather than a decision.
    ///
    /// After a vote, the safe thing is the opposite - leave the super-journal
    /// where it is, so that every vote already on disk is a vote that lost - and
    /// that is what simply dropping the handle does, because this type has no
    /// destructor.
    pub fn abandon(self) {
        for marker in &self.markers {
            let _ = std::fs::remove_file(marker);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Writes a small file and makes it durable before returning.
///
/// @param path - where it goes
/// @param bytes - what it holds
fn write_durably(path: &std::path::Path, bytes: &[u8]) -> DbResult<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)
        .map_err(|error| misuse(format!("cannot write {}: {error}", path.display())))?;
    file.write_all(bytes)
        .map_err(|error| misuse(format!("cannot write {}: {error}", path.display())))?;
    file.sync_all()
        .map_err(|error| misuse(format!("cannot sync {}: {error}", path.display())))?;
    Ok(())
}
