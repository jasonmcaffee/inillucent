//! The three answers a virtual-table module gives about its own storage.
//!
//! Invariant: **the answer is a named variant and not a nest of options.** This
//! was `Option<Option<String>>` on three signatures until task-1961's A10, and
//! the outer and the inner `None` mean different things - no such table, and a
//! table that checked itself and found nothing wrong. The only place that
//! destructured it was `rtree_check`, four hundred lines away from the three
//! places that built it.

/// What a module answered when it was asked to check its own storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleIntegrity {
    /// No table of that name, or its module does not check itself.
    NoSuchModule,
    /// The module checked its storage and found nothing wrong.
    Clean,
    /// The module checked its storage and this is what it found.
    Report(String),
}

impl ModuleIntegrity {
    /// Returns a module's own answer as one of the three.
    ///
    /// `integrity` answers `None` for "nothing wrong", which is the trait's
    /// shape and not this one's.
    ///
    /// @param found - what the module's `integrity` answered
    pub fn of(found: Option<String>) -> ModuleIntegrity {
        match found {
            None => ModuleIntegrity::Clean,
            Some(report) => ModuleIntegrity::Report(report),
        }
    }
}
