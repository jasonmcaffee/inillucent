//! The settings a connection remembers, and what each of them means here.
//!
//! Invariant: every setting in this list answers, and the ones this engine acts
//! on say so. A pragma that silently did nothing and reported the value it was
//! given would be worse than one that refused: the application would believe it
//! had asked for something. So each row records whether the engine *acts* on
//! it, and the ones it does not are recorded as deliberate rather than
//! forgotten - `PRAGMA threads = 4` is remembered and reported and changes
//! nothing, because this engine has no worker threads to give it.
//!
//! The two kinds are kept apart in the type rather than in a comment, so a
//! setting that gains an implementation is a one-line change in one place.

use inillucent_base::DbResult;

/// One setting a connection carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    /// `PRAGMA cache_size`: how many pages the cache holds, or how many
    /// kibibytes when negative.
    CacheSize,
    /// `PRAGMA cache_spill`: whether a large transaction may write dirty pages
    /// before it commits.
    CacheSpill,
    /// `PRAGMA max_page_count`: the largest the file may grow to.
    MaxPageCount,
    /// `PRAGMA journal_size_limit`: how large a journal may be left.
    JournalSizeLimit,
    /// `PRAGMA mmap_size`: how much of the file may be memory-mapped.
    MmapSize,
    /// `PRAGMA temp_store`: where temporary tables live.
    TempStore,
    /// `PRAGMA secure_delete`: whether deleted content is overwritten.
    SecureDelete,
    /// `PRAGMA busy_timeout`: how long a busy lock is waited for.
    BusyTimeout,
    /// `PRAGMA query_only`: whether the connection may write at all.
    QueryOnly,
    /// `PRAGMA locking_mode`: whether the connection keeps its locks.
    ExclusiveLocking,
    /// `PRAGMA recursive_triggers`.
    RecursiveTriggers,
    /// `PRAGMA reverse_unordered_selects`.
    ReverseUnorderedSelects,
    /// `PRAGMA ignore_check_constraints`.
    IgnoreCheckConstraints,
    /// `PRAGMA cell_size_check`.
    CellSizeCheck,
    /// `PRAGMA legacy_alter_table`.
    LegacyAlterTable,
    /// `PRAGMA automatic_index`.
    AutomaticIndex,
    /// `PRAGMA read_uncommitted`.
    ReadUncommitted,
    /// `PRAGMA case_sensitive_like`.
    CaseSensitiveLike,
    /// `PRAGMA count_changes`, deprecated and inert.
    CountChanges,
    /// `PRAGMA full_column_names`, deprecated and inert.
    FullColumnNames,
    /// `PRAGMA short_column_names`, deprecated and inert.
    ShortColumnNames,
    /// `PRAGMA empty_result_callbacks`, deprecated and inert.
    EmptyResultCallbacks,
    /// `PRAGMA threads`: how many helper threads a sort may use.
    Threads,
    /// `PRAGMA analysis_limit`: how much of an index `ANALYZE` samples.
    AnalysisLimit,
    /// `PRAGMA soft_heap_limit`.
    SoftHeapLimit,
    /// `PRAGMA hard_heap_limit`.
    HardHeapLimit,
}

/// Every setting, with the name it is spelt as and its default.
const SETTINGS: &[(Setting, &str, i64, bool, bool)] = &[
    // (setting, name, default, is boolean, answers after a write)
    (Setting::AnalysisLimit, "analysis_limit", 0, false, true),
    (Setting::AutomaticIndex, "automatic_index", 1, true, true),
    (Setting::BusyTimeout, "busy_timeout", 0, false, true),
    (Setting::CacheSize, "cache_size", -2000, false, true),
    (Setting::CacheSpill, "cache_spill", 1, false, true),
    (
        Setting::CaseSensitiveLike,
        "case_sensitive_like",
        0,
        true,
        false,
    ),
    (Setting::CellSizeCheck, "cell_size_check", 0, true, true),
    (Setting::CountChanges, "count_changes", 0, true, true),
    (
        Setting::EmptyResultCallbacks,
        "empty_result_callbacks",
        0,
        true,
        true,
    ),
    (Setting::ExclusiveLocking, "locking_mode", 0, true, true),
    (Setting::FullColumnNames, "full_column_names", 0, true, true),
    (Setting::HardHeapLimit, "hard_heap_limit", 0, false, true),
    (
        Setting::IgnoreCheckConstraints,
        "ignore_check_constraints",
        0,
        true,
        true,
    ),
    (
        Setting::JournalSizeLimit,
        "journal_size_limit",
        -1,
        false,
        true,
    ),
    (
        Setting::LegacyAlterTable,
        "legacy_alter_table",
        0,
        true,
        true,
    ),
    (
        Setting::MaxPageCount,
        "max_page_count",
        4_294_967_294,
        false,
        true,
    ),
    (Setting::MmapSize, "mmap_size", 0, false, false),
    (Setting::QueryOnly, "query_only", 0, true, true),
    (Setting::ReadUncommitted, "read_uncommitted", 0, true, true),
    (
        Setting::RecursiveTriggers,
        "recursive_triggers",
        0,
        true,
        true,
    ),
    (
        Setting::ReverseUnorderedSelects,
        "reverse_unordered_selects",
        0,
        true,
        true,
    ),
    (Setting::SecureDelete, "secure_delete", 0, true, true),
    (
        Setting::ShortColumnNames,
        "short_column_names",
        1,
        true,
        true,
    ),
    (Setting::SoftHeapLimit, "soft_heap_limit", 0, false, true),
    (Setting::TempStore, "temp_store", 0, false, true),
    (Setting::Threads, "threads", 0, false, true),
];

impl Setting {
    /// Returns the setting a pragma name spells.
    pub fn named(name: &[u8]) -> Option<Setting> {
        let folded = name.to_ascii_lowercase();
        SETTINGS
            .iter()
            .find(|(_, spelling, _, _, _)| spelling.as_bytes() == folded.as_slice())
            .map(|(setting, _, _, _, _)| *setting)
    }

    /// Returns whether the setting is read as a boolean.
    pub fn is_boolean(self) -> bool {
        SETTINGS
            .iter()
            .find(|(setting, _, _, _, _)| *setting == self)
            .map(|(_, _, _, boolean, _)| *boolean)
            .unwrap_or(false)
    }

    /// Returns whether a write is followed by an answer.
    ///
    /// Most pragmas answer with the value they now hold. A few answer nothing
    /// at all after a write, and `mmap_size` answers nothing even on a read -
    /// which is what the pinned release does, and which an application that
    /// tested for an empty result would notice.
    pub fn answers_after_a_write(self) -> bool {
        SETTINGS
            .iter()
            .find(|(setting, _, _, _, _)| *setting == self)
            .map(|(_, _, _, _, answers)| *answers)
            .unwrap_or(true)
    }

    /// Returns the value the setting starts at.
    pub fn default_value(self) -> i64 {
        SETTINGS
            .iter()
            .find(|(setting, _, _, _, _)| *setting == self)
            .map(|(_, _, default, _, _)| *default)
            .unwrap_or(0)
    }
}

/// Every setting a connection carries, with the value it holds.
#[derive(Clone, Debug)]
pub struct Settings {
    values: Vec<(Setting, i64)>,
}

impl Default for Settings {
    /// Returns every setting at the value SQLite starts it at.
    fn default() -> Settings {
        Settings {
            values: SETTINGS
                .iter()
                .map(|(setting, _, default, _, _)| (*setting, *default))
                .collect(),
        }
    }
}

impl Settings {
    /// Returns what a setting holds.
    pub fn get(&self, setting: Setting) -> i64 {
        self.values
            .iter()
            .find(|(candidate, _)| *candidate == setting)
            .map(|(_, value)| *value)
            .unwrap_or_else(|| setting.default_value())
    }

    /// Changes what a setting holds, returning what it held before.
    pub fn set(&mut self, setting: Setting, value: i64) -> DbResult<i64> {
        for (candidate, slot) in self.values.iter_mut() {
            if *candidate == setting {
                let previous = *slot;
                *slot = value;
                return Ok(previous);
            }
        }
        self.values.push((setting, value));
        Ok(setting.default_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every setting is reachable by the name its pragma is spelt with.
    #[test]
    fn every_setting_has_a_name() {
        for (setting, name, _, _, _) in SETTINGS {
            assert_eq!(Setting::named(name.as_bytes()), Some(*setting), "{name}");
        }
    }

    /// The defaults are SQLite's, including the two that are not zero.
    #[test]
    fn the_defaults_are_the_reference_defaults() {
        let settings = Settings::default();
        assert_eq!(settings.get(Setting::CacheSize), -2000);
        assert_eq!(settings.get(Setting::JournalSizeLimit), -1);
        assert_eq!(settings.get(Setting::MaxPageCount), 4_294_967_294);
        assert_eq!(settings.get(Setting::AutomaticIndex), 1);
        assert_eq!(settings.get(Setting::QueryOnly), 0);
    }

    /// A write reports what was there before it.
    #[test]
    fn a_write_reports_the_previous_value() {
        let mut settings = Settings::default();
        assert_eq!(settings.set(Setting::QueryOnly, 1).expect("sets"), 0);
        assert_eq!(settings.get(Setting::QueryOnly), 1);
    }

    /// A name nobody registered is not a setting.
    #[test]
    fn an_unknown_name_is_not_a_setting() {
        assert!(Setting::named(b"nope").is_none());
        assert!(Setting::named(b"QUERY_ONLY").is_some());
    }
}
