//! Run-time limits.
//!
//! Invariant: a limit is never read from a literal at a call site. Every
//! consumer asks a `Limits` value, and every `Limits` value was clamped into
//! the range `compat/limits.toml` declares, so a caller cannot raise a limit
//! past what the engine can actually honour.
//!
//! SQLite's `sqlite3_limit()` returns the previous value and clamps rather than
//! failing, including for a negative argument, which means "query the current
//! value". inillucent reproduces that shape exactly because callers rely on it.

extern crate alloc;

mod generated {
    //! The table generated from `compat/limits.toml` at build time.
    #![allow(missing_docs)]
    include!(concat!(env!("OUT_DIR"), "/limits_generated.rs"));
}

pub use generated::{Limit, LimitRow, LIMIT_ROWS};

impl Limit {
    /// Returns the generated row for this limit.
    ///
    /// Every variant is generated from the manifest, so the lookup always
    /// succeeds; the fallback keeps the function total without an `unwrap`.
    pub fn row(self) -> &'static LimitRow {
        match LIMIT_ROWS.iter().find(|row| row.limit == self) {
            Some(row) => row,
            None => &UNRECOGNISED_LIMIT_ROW,
        }
    }

    /// Returns the value a fresh connection starts with.
    pub fn default_value(self) -> i64 {
        self.row().default
    }

    /// Returns the ceiling a caller may not raise this limit past.
    pub fn hard_max(self) -> i64 {
        self.row().hard_max
    }

    /// Returns the floor a caller may not lower this limit past.
    pub fn minimum(self) -> i64 {
        self.row().minimum
    }

    /// Returns the C macro name.
    pub fn c_name(self) -> &'static str {
        self.row().c_name
    }

    /// Clamps a requested value into this limit's legal range.
    pub fn clamp(self, requested: i64) -> i64 {
        requested.clamp(self.minimum(), self.hard_max())
    }
}

/// The row returned for a limit the generated table does not list; unreachable
/// in practice because the enum itself is generated from that table.
static UNRECOGNISED_LIMIT_ROW: LimitRow = LimitRow {
    limit: Limit::Length,
    c_name: "SQLITE_LIMIT_LENGTH",
    default: 1_000_000_000,
    hard_max: 2_147_483_645,
    minimum: 0,
    description: "Maximum size of any string, BLOB, or table row in bytes.",
};

/// One connection's current limit values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Limits {
    /// The current value of every limit, in manifest order.
    ///
    /// Shared rather than owned, because a `Limits` is *cloned* far more often
    /// than it is changed. The machine clones one on nine paths, several of
    /// them per column of per row - and while it held a `Vec` that was a heap
    /// allocation and a free for every column any query read, which measured at
    /// a fifth of the cost of reading the column itself. Setting a limit is a
    /// `PRAGMA` or an `sqlite3_limit` call and can afford to copy.
    values: alloc::sync::Arc<[i64]>,
}

impl Default for Limits {
    /// Starts every limit at its manifest default.
    fn default() -> Limits {
        Limits {
            values: LIMIT_ROWS
                .iter()
                .map(|row| row.default)
                .collect::<Vec<i64>>()
                .into(),
        }
    }
}

impl Limits {
    /// Returns the current value of one limit.
    pub fn get(&self, limit: Limit) -> i64 {
        match self
            .index_of(limit)
            .and_then(|index| self.values.get(index))
        {
            Some(value) => *value,
            None => limit.default_value(),
        }
    }

    /// Sets a limit and returns its previous value.
    ///
    /// A negative request is a query, matching `sqlite3_limit`; anything else
    /// is clamped into the manifest range rather than rejected.
    pub fn set(&mut self, limit: Limit, requested: i64) -> i64 {
        let previous = self.get(limit);
        if requested < 0 {
            return previous;
        }
        if let Some(index) = self.index_of(limit) {
            let mut owned = self.values.to_vec();
            if let Some(slot) = owned.get_mut(index) {
                *slot = limit.clamp(requested);
            }
            self.values = owned.into();
        }
        previous
    }

    /// Reports whether a length fits inside the `Length` limit.
    pub fn permits_length(&self, length: u64) -> bool {
        length <= self.get(Limit::Length).max(0) as u64
    }

    /// Returns the position of a limit in the generated table.
    fn index_of(&self, limit: Limit) -> Option<usize> {
        LIMIT_ROWS.iter().position(|row| row.limit == limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults must match the reference build; a silently different
    /// default is a parity deviation nobody would notice until a query failed.
    #[test]
    fn defaults_match_the_reference_build() {
        let limits = Limits::default();
        assert_eq!(limits.get(Limit::Length), 1_000_000_000);
        assert_eq!(limits.get(Limit::Column), 2000);
        assert_eq!(limits.get(Limit::VariableNumber), 32_766);
        assert_eq!(limits.get(Limit::Attached), 10);
        // 127 until SQLite raised it; 3.53.4 compiles SQLITE_MAX_FUNCTION_ARG
        // as 1000, and the oracle reports that from sqlite3_limit(). This
        // number was 127 here until the phase 2 differential run asked the
        // reference build for all twelve limits and compared them.
        assert_eq!(limits.get(Limit::FunctionArg), 1000);
    }

    /// Setting returns the previous value and clamps rather than failing.
    #[test]
    fn setting_returns_the_previous_value_and_clamps() {
        let mut limits = Limits::default();
        assert_eq!(limits.set(Limit::Column, 50), 2000);
        assert_eq!(limits.get(Limit::Column), 50);
        assert_eq!(limits.set(Limit::Column, 1_000_000), 50);
        assert_eq!(limits.get(Limit::Column), Limit::Column.hard_max());
        assert_eq!(limits.set(Limit::Column, 0), Limit::Column.hard_max());
        assert_eq!(limits.get(Limit::Column), Limit::Column.minimum());
    }

    /// A negative argument queries without changing anything.
    #[test]
    fn a_negative_request_only_queries() {
        let mut limits = Limits::default();
        assert_eq!(limits.set(Limit::Attached, -1), 10);
        assert_eq!(limits.get(Limit::Attached), 10);
    }

    /// Every manifest row must be internally consistent, or clamping would
    /// produce a value outside its own range.
    #[test]
    fn every_row_has_a_coherent_range() {
        for row in LIMIT_ROWS.iter() {
            assert!(row.minimum <= row.hard_max, "{}", row.c_name);
            assert!(row.default >= row.minimum, "{}", row.c_name);
            assert!(row.default <= row.hard_max, "{}", row.c_name);
            assert_eq!(row.limit.c_name(), row.c_name);
        }
    }
}
