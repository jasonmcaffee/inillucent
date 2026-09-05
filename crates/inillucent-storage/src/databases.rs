//! The databases one statement can reach.
//!
//! Invariant: a statement addresses a database by number, and the number means
//! the same thing everywhere - in the catalog the statement was bound against,
//! in the instruction that opens a cursor, and in whatever holds the pagers.
//! `main` is always zero. Everything else is where the connection put it, and
//! the connection is the only thing allowed to decide.
//!
//! This is a trait rather than a container because storage has no business
//! knowing what a database is *called*. A name is schema, the session owns it,
//! and what storage needs is the one question a running statement asks: which
//! pager does database three mean.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;

use crate::pager::Pager;

/// The number every statement uses for the database it was opened on.
pub const MAIN_DATABASE: usize = 0;

/// The number the connection's temporary database always has.
///
/// It is fixed rather than assigned, and it is one for the same reason SQLite
/// makes it one: a statement bound before an `ATTACH` carries the numbers it
/// resolved against, and a temporary database that moved when something was
/// attached would move underneath them. Nothing is attached at one; the
/// attached databases start at two.
pub const TEMP_DATABASE: usize = 1;

/// The databases a statement can reach, addressed by number.
pub trait PagerSet {
    /// Returns the pager of one attached database.
    fn pager(&mut self, database: usize) -> DbResult<&mut Pager>;

    /// Returns how many databases are attached.
    fn count(&self) -> usize;
}

impl PagerSet for Pager {
    /// A single database is a set of one, which is what every statement that
    /// never mentions a schema is running against.
    fn pager(&mut self, database: usize) -> DbResult<&mut Pager> {
        if database != MAIN_DATABASE {
            return Err(misuse(format!(
                "database {database} is not attached to this connection"
            )));
        }
        Ok(self)
    }

    /// One.
    fn count(&self) -> usize {
        1
    }
}

impl PagerSet for Vec<Pager> {
    /// The pagers in the order the connection attached them.
    fn pager(&mut self, database: usize) -> DbResult<&mut Pager> {
        self.get_mut(database).ok_or_else(|| {
            misuse(format!(
                "database {database} is not attached to this connection"
            ))
        })
    }

    /// How many are attached.
    fn count(&self) -> usize {
        self.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pager::PagerOptions;
    use inillucent_vfs::{DbPath, MemoryVfs};

    /// Builds an empty database in memory.
    fn database(vfs: &MemoryVfs, name: &str) -> Pager {
        Pager::create(
            vfs,
            &DbPath::new(std::path::PathBuf::from(name)),
            PagerOptions::default(),
            crate::pager::NewDatabase::default(),
        )
        .expect("the database is created")
    }

    /// One pager answers for `main` and refuses every other number, which is
    /// what makes a statement that mentions a schema fail loudly on a
    /// connection that has only one.
    #[test]
    fn one_pager_is_a_set_of_one() {
        let vfs = MemoryVfs::new();
        let mut pager = database(&vfs, "/one.db");
        assert_eq!(PagerSet::count(&pager), 1);
        assert!(pager.pager(MAIN_DATABASE).is_ok());
        assert!(pager.pager(1).is_err());
    }

    /// A list answers by position, and a number past the end is a mistake
    /// rather than a silent read of the wrong database.
    #[test]
    fn a_list_answers_by_position() {
        let vfs = MemoryVfs::new();
        let mut set = vec![database(&vfs, "/main.db"), database(&vfs, "/other.db")];
        assert_eq!(PagerSet::count(&set), 2);
        assert_eq!(
            set.pager(0).map(|pager| pager.path().display()),
            Ok("/main.db".to_string())
        );
        assert_eq!(
            set.pager(1).map(|pager| pager.path().display()),
            Ok("/other.db".to_string())
        );
        assert!(set.pager(2).is_err());
    }
}
