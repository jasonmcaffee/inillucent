//! What a reader is allowed to do with bytes somebody damaged.
//!
//! Invariant: **a decoder reading damaged bytes either answers exactly what the
//! undamaged bytes answered, or refuses with a status in the corruption
//! family.** Anything else is a wrong answer: a value a caller cannot tell from
//! a right one, produced by a page that passed whatever checks it has.
//!
//! The rule is weaker than "it must notice", deliberately. A decoder is obliged
//! to be *safe* on bytes that pass its checks; being *right* about them is what
//! the checksum over the page is for, and that is a different test. So a
//! damaged page that still decodes to the same thing is fine, and a damaged
//! page that decodes to something else is not.
//!
//! It lives here rather than in one test file because three suites need the
//! same judgement and were making it three ways: `corruption.rs` over the
//! SQLite format through the storage pager, `phase2_campaigns.rs` over the
//! native format's pages, and `fault_shapes.rs` over a whole database
//! (task-2066 section 4.4.6).

use inillucent_base::error::PrimaryCode;
use inillucent_base::DbError;

/// The statuses a damaged file is allowed to be refused with.
///
/// `NoMem` and `TooBig` are in the list because a length read out of a damaged
/// header is what the allocator is asked for, and refusing the allocation is
/// the right answer to it - the alternative is aborting the process, which is
/// what task-2066 section 4.1.12 found the search readers doing.
const FAMILY: [PrimaryCode; 5] = [
    PrimaryCode::Corrupt,
    PrimaryCode::NotADb,
    PrimaryCode::IoErr,
    PrimaryCode::TooBig,
    PrimaryCode::NoMem,
];

/// Reports whether a refusal is one a damaged file may produce.
///
/// @param error - what the reader said
pub fn in_the_corruption_family(error: &DbError) -> bool {
    FAMILY.contains(&error.code())
}

/// Asserts a refusal is one a damaged file may produce.
///
/// @param error - what the reader said
/// @param fixture - which fixture was damaged, for the message
/// @param case - which damaged copy of it, for the message
pub fn assert_expected(error: &DbError, fixture: &str, case: u32) {
    assert!(
        in_the_corruption_family(error),
        "{fixture} case {case} failed with {:?}, which is not a corruption family",
        error.code()
    );
}

/// How one sweep of damaged inputs came out.
///
/// **Both halves are counted, and that is the point** (task-2066 section
/// 4.4.6). A sweep that only counted refusals would report a rate that falls
/// when the reader gets *better* at reading damaged bytes correctly, and a
/// sweep that only counted panics would report a rate of zero for a reader that
/// answers wrongly every time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Sweep {
    /// How many damaged inputs were read.
    pub total: u32,
    /// How many were refused, with a status in the corruption family.
    pub refused: u32,
    /// How many answered exactly what the undamaged input answered.
    pub unchanged: u32,
}

impl Sweep {
    /// Records one damaged input that was refused.
    pub fn refuse(&mut self) {
        self.total = self.total.saturating_add(1);
        self.refused = self.refused.saturating_add(1);
    }

    /// Records one damaged input that answered what the original answered.
    pub fn unchanged(&mut self) {
        self.total = self.total.saturating_add(1);
        self.unchanged = self.unchanged.saturating_add(1);
    }

    /// Returns the share of inputs that were refused, 0.0 to 1.0.
    ///
    /// Zero for an empty sweep rather than a division by zero: an empty sweep
    /// is a sweep that measured nothing, and the caller asserts against that
    /// separately.
    pub fn detection_rate(&self) -> f64 {
        match self.total {
            0 => 0.0,
            total => f64::from(self.refused) / f64::from(total),
        }
    }

    /// Returns how many inputs were neither refused nor unchanged.
    ///
    /// Every one of these is a wrong answer, and the number is what a caller
    /// asserts is zero.
    pub fn wrong(&self) -> u32 {
        self.total
            .saturating_sub(self.refused)
            .saturating_sub(self.unchanged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The family holds the five statuses and refuses a sixth.
    #[test]
    fn the_family_is_the_five_statuses_and_no_others() {
        for code in FAMILY {
            assert!(in_the_corruption_family(&DbError::primary(code)));
        }
        assert!(!in_the_corruption_family(&DbError::primary(
            PrimaryCode::Constraint
        )));
        assert!(!in_the_corruption_family(&DbError::primary(
            PrimaryCode::Misuse
        )));
    }

    /// A sweep counts both halves, and what is neither is wrong.
    #[test]
    fn a_sweep_counts_refused_unchanged_and_wrong() {
        let mut sweep = Sweep::default();
        assert_eq!(
            sweep.detection_rate(),
            0.0,
            "an empty sweep measures nothing"
        );
        sweep.refuse();
        sweep.refuse();
        sweep.refuse();
        sweep.unchanged();
        assert_eq!(sweep.total, 4);
        assert_eq!(sweep.detection_rate(), 0.75);
        assert_eq!(sweep.wrong(), 0);
        // A total that outruns the two counters is the wrong answers, which is
        // how a caller that records one without the other is caught.
        sweep.total = sweep.total.saturating_add(2);
        assert_eq!(sweep.wrong(), 2);
    }
}
