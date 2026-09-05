//! Copying a database into another one, a few pages at a time.
//!
//! Invariant: the copy is of one snapshot. The source is held in a read
//! transaction from the first step to the last, so a writer that commits
//! half-way through does not put half of one database and half of another into
//! the destination. SQLite instead lets the source move and restarts the copy;
//! holding the snapshot is the simpler promise and the one worth making here,
//! and what it costs is that a long backup keeps a reader on the source - which
//! in rollback mode delays a writer's commit and in WAL mode delays nothing.
//!
//! The destination is written page for page rather than rebuilt. A backup is
//! not a `VACUUM`: it reproduces the file, free pages and all, so what comes
//! out is byte-identical to what went in and can be compared as such.

use inillucent_base::error::misuse;
use inillucent_base::ids::PageId;
use inillucent_base::DbResult;

use crate::connection::{Access, Connection, Outcome};

/// How much of a backup is left.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BackupProgress {
    /// How many pages the source has.
    pub page_count: u32,
    /// How many of them have not been copied yet.
    pub remaining: u32,
}

impl BackupProgress {
    /// Reports whether every page has been copied.
    pub fn is_complete(&self) -> bool {
        self.remaining == 0
    }
}

/// A copy in progress from one database to another.
pub struct Backup<'a> {
    source: &'a Connection,
    source_database: usize,
    destination: &'a Connection,
    destination_database: usize,
    next_page: u32,
    page_count: u32,
    open: bool,
}

impl<'a> Backup<'a> {
    /// Starts a backup, taking the snapshot it will copy.
    ///
    /// The two databases must agree about their page size. SQLite changes the
    /// destination's when it is empty; this refuses instead, because a
    /// destination whose page size was chosen by the caller and then silently
    /// replaced is a surprise, and the caller that wants the source's page size
    /// can create the destination with it.
    pub fn begin(
        source: &'a Connection,
        source_database: usize,
        destination: &'a Connection,
        destination_database: usize,
    ) -> DbResult<Backup<'a>> {
        if core::ptr::eq(source, destination) && source_database == destination_database {
            return Err(misuse("a database cannot be backed up onto itself"));
        }
        source.begin_statement(Access::Read)?;
        let started = Backup::measure(source, source_database, destination, destination_database);
        let (page_count, page_size) = match started {
            Ok(measured) => measured,
            Err(error) => {
                let _ = source.end_statement(Access::Read, Outcome::Abort);
                return Err(error);
            }
        };
        let _ = page_size;
        Ok(Backup {
            source,
            source_database,
            destination,
            destination_database,
            next_page: 1,
            page_count,
            open: true,
        })
    }

    /// Reads the source's size and checks the two page sizes agree.
    fn measure(
        source: &Connection,
        source_database: usize,
        destination: &Connection,
        destination_database: usize,
    ) -> DbResult<(u32, u32)> {
        let source_size =
            source.with_database(source_database, |pager| pager.page_size().bytes())?;
        let destination_size =
            destination.with_database(destination_database, |pager| pager.page_size().bytes())?;
        if source_size != destination_size {
            return Err(misuse(format!(
                "a backup needs the same page size at both ends: {source_size} and {destination_size}"
            )));
        }
        let page_count = source.with_database(source_database, |pager| pager.page_count())?;
        Ok((page_count, source_size))
    }

    /// Returns how much is left to do.
    pub fn progress(&self) -> BackupProgress {
        BackupProgress {
            page_count: self.page_count,
            remaining: self
                .page_count
                .saturating_sub(self.next_page.saturating_sub(1)),
        }
    }

    /// Copies up to `pages` pages, or every remaining one when it is negative.
    ///
    /// Each step is its own write transaction on the destination, which is what
    /// makes a backup interruptible: stopping between two steps leaves a
    /// destination that is a valid database of the pages copied so far rather
    /// than a half-written file.
    pub fn step(&mut self, pages: i32) -> DbResult<BackupProgress> {
        if !self.open {
            return Err(misuse("this backup has finished"));
        }
        let wanted = if pages < 0 {
            self.page_count
        } else {
            pages.max(0) as u32
        };
        let last = self
            .next_page
            .saturating_add(wanted)
            .min(self.page_count.saturating_add(1));
        if self.next_page >= last {
            return Ok(self.progress());
        }
        let images = self.read_pages(self.next_page, last)?;
        self.write_pages(self.next_page, &images)?;
        self.next_page = last;
        Ok(self.progress())
    }

    /// Reads a run of pages out of the source's snapshot.
    fn read_pages(&self, first: u32, last: u32) -> DbResult<Vec<Vec<u8>>> {
        let mut images = Vec::with_capacity(last.saturating_sub(first) as usize);
        for page in first..last {
            let image = self.source.with_database(self.source_database, |pager| {
                let id = PageId::from_persisted(page)?;
                Ok::<Vec<u8>, inillucent_base::DbError>(pager.get_page(id)?.bytes().to_vec())
            })??;
            images.push(image);
        }
        Ok(images)
    }

    /// Writes a run of pages into the destination, in one transaction.
    fn write_pages(&self, first: u32, images: &[Vec<u8>]) -> DbResult<()> {
        self.destination
            .begin_statement_on(Access::Schema, &[self.destination_database])?;
        let outcome = self.write_pages_inside(first, images);
        let ending = if outcome.is_ok() {
            Outcome::Done
        } else {
            Outcome::Abort
        };
        let closed = self.destination.end_statement(Access::Schema, ending);
        outcome?;
        closed
    }

    /// Writes the pages with the destination's transaction already open.
    fn write_pages_inside(&self, first: u32, images: &[Vec<u8>]) -> DbResult<()> {
        let wanted = first.saturating_add(images.len() as u32).saturating_sub(1);
        self.destination
            .with_database(self.destination_database, |pager| {
                if pager.page_count() < wanted {
                    pager.set_page_count(wanted)?;
                }
                for (offset, image) in images.iter().enumerate() {
                    let number = first.saturating_add(offset as u32);
                    let id = PageId::from_persisted(number)?;
                    pager.edit_page(id, |raw| {
                        let len = raw.len().min(image.len());
                        if let (Some(target), Some(source)) = (raw.get_mut(..len), image.get(..len))
                        {
                            target.copy_from_slice(source);
                        }
                        Ok(())
                    })?;
                }
                Ok::<(), inillucent_base::DbError>(())
            })?
    }

    /// Finishes the backup, shortening the destination to the source's size.
    ///
    /// The truncation is the last step and it is part of the copy: a
    /// destination that was longer than the source would be a database with
    /// pages past its own end, which every reader would take as corruption.
    pub fn finish(mut self) -> DbResult<()> {
        self.open = false;
        let complete = self.next_page > self.page_count;
        let truncated = if complete {
            self.truncate_destination()
        } else {
            Ok(())
        };
        let released = self.source.end_statement(Access::Read, Outcome::Done);
        truncated?;
        released?;
        self.destination.reload_schema()
    }

    /// Shortens the destination and stamps the source's header onto it.
    fn truncate_destination(&self) -> DbResult<()> {
        let header = self
            .source
            .with_database(self.source_database, |pager| *pager.header())?;
        self.destination
            .begin_statement_on(Access::Schema, &[self.destination_database])?;
        let outcome = self
            .destination
            .with_database(self.destination_database, |pager| {
                pager.set_page_count(self.page_count)?;
                // The change counter and the version it was valid for are left
                // to the destination's own commit: a copy has to differ in
                // those, because they are how another connection learns its
                // cache is stale.
                pager.set_header(header)
            })?;
        let ending = if outcome.is_ok() {
            Outcome::Done
        } else {
            Outcome::Abort
        };
        let closed = self.destination.end_statement(Access::Schema, ending);
        outcome?;
        closed
    }

    /// Abandons the backup, leaving the destination as the last step left it.
    pub fn abandon(mut self) -> DbResult<()> {
        self.open = false;
        self.source.end_statement(Access::Read, Outcome::Abort)
    }
}

impl Drop for Backup<'_> {
    /// Releases the source's snapshot even when a caller forgot to finish.
    ///
    /// A read transaction left open holds a lock, and a backup abandoned
    /// halfway is a normal thing for an application to do.
    fn drop(&mut self) {
        if self.open {
            let _ = self.source.end_statement(Access::Read, Outcome::Abort);
        }
    }
}
