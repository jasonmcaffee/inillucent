//! Systematic failure injection.
//!
//! Invariant: a failpoint fires only where the policy says it should, and the
//! same policy plus the same seed fires at the same call every time. A failure
//! that cannot be replayed cannot be fixed.
//!
//! The campaign the assurance plan describes is "fail the Nth call, for
//! N = 1, 2, 3 ... until a run completes without reaching call N". That needs a
//! global counter over every injectable site rather than a per-site one, which
//! is why `FailNth` counts sites and `sites_reached` reports how far a run got.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use inillucent_base::rng::Rng;
use inillucent_vfs::error::{self, VfsError, VfsOperation};

/// Where a failure can be injected.
///
/// A site is the operation, not the call: `Write` covers every write, which is
/// what makes an "Nth call" campaign enumerate an execution rather than a
/// hand-written list.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Site {
    /// Opening a file.
    Open,
    /// Reading from a file.
    Read,
    /// Writing to a file.
    Write,
    /// Flushing a file.
    Sync,
    /// Changing a file's length.
    Truncate,
    /// Taking a lock.
    Lock,
    /// Deleting a file.
    Delete,
    /// Mapping or growing shared memory.
    Shm,
    /// Allocating a buffer.
    Allocate,
}

impl Site {
    /// Returns every site, so a campaign can enumerate them.
    pub fn all() -> [Site; 9] {
        [
            Site::Open,
            Site::Read,
            Site::Write,
            Site::Sync,
            Site::Truncate,
            Site::Lock,
            Site::Delete,
            Site::Shm,
            Site::Allocate,
        ]
    }

    /// Returns the VFS operation a failure at this site is reported as.
    pub fn operation(self) -> VfsOperation {
        match self {
            Site::Open => VfsOperation::Open,
            Site::Read => VfsOperation::Read,
            Site::Write => VfsOperation::Write,
            Site::Sync => VfsOperation::Sync,
            Site::Truncate => VfsOperation::Truncate,
            Site::Lock => VfsOperation::Lock,
            Site::Delete => VfsOperation::Delete,
            Site::Shm => VfsOperation::ShmMap,
            Site::Allocate => VfsOperation::Read,
        }
    }
}

/// What a fired failpoint does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    /// The operation reports an I/O error for its site.
    IoError,
    /// The operation reports that the disk is full.
    DiskFull,
    /// A read returns fewer bytes than asked for.
    ShortRead,
    /// A write stores fewer bytes than asked for and reports success, which is
    /// the nastiest of the failures because nothing complains at the time.
    ShortWrite,
    /// The operation reports that it was interrupted.
    Interrupt,
    /// The operation reports that permission was denied.
    Permission,
    /// The simulated machine loses power at this point.
    Crash,
}

impl Failure {
    /// Builds the error a fired failpoint returns, or `None` for the failures
    /// that are not reported as an error.
    pub fn to_error(self, site: Site) -> Option<VfsError> {
        match self {
            Failure::IoError => Some(VfsError::new(
                site.operation().extended_code(),
                "injected I/O error",
            )),
            Failure::DiskFull => Some(error::disk_full("injected disk-full")),
            Failure::ShortRead => Some(error::short_read("injected short read")),
            Failure::Interrupt => Some(error::interrupted("injected interrupt")),
            Failure::Permission => Some(VfsError::new(
                inillucent_base::error::ExtendedCode::from_primary(
                    inillucent_base::error::PrimaryCode::Perm,
                ),
                "injected permission failure",
            )),
            Failure::ShortWrite | Failure::Crash => None,
        }
    }
}

/// When a failpoint fires.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Policy {
    /// Never.
    Off,
    /// On the `n`th time this site is reached, counting from one.
    Nth(u64, Failure),
    /// Every time this site is reached.
    Always(Failure),
    /// With probability `numerator / denominator`, decided by the run's seed.
    Chance(u64, u64, Failure),
}

/// The failpoint table for one simulated run.
#[derive(Debug)]
pub struct Failpoints {
    policies: Mutex<BTreeMap<Site, Policy>>,
    counts: Mutex<BTreeMap<Site, u64>>,
    sites_reached: AtomicU64,
    fail_nth: AtomicU64,
    nth_failure: Mutex<Failure>,
    rng: Mutex<Rng>,
}

impl Failpoints {
    /// Creates a table that injects nothing, seeded for the chance policies.
    pub fn new(seed: u64) -> Failpoints {
        Failpoints {
            policies: Mutex::new(BTreeMap::new()),
            counts: Mutex::new(BTreeMap::new()),
            sites_reached: AtomicU64::new(0),
            fail_nth: AtomicU64::new(0),
            nth_failure: Mutex::new(Failure::IoError),
            rng: Mutex::new(Rng::new(seed)),
        }
    }

    /// Sets the policy for one site.
    pub fn set(&self, site: Site, policy: Policy) {
        guard(&self.policies).insert(site, policy);
    }

    /// Arms the systematic campaign: fail the `n`th injectable call of the run,
    /// whichever site it turns out to be. `n` of zero disarms it.
    pub fn fail_nth_call(&self, n: u64, failure: Failure) {
        self.fail_nth.store(n, Ordering::Relaxed);
        *guard(&self.nth_failure) = failure;
    }

    /// Returns how many injectable calls the run has made.
    ///
    /// A campaign stops when this stops growing past the `n` it is testing:
    /// the execution completed without reaching a new failpoint.
    pub fn sites_reached(&self) -> u64 {
        self.sites_reached.load(Ordering::Relaxed)
    }

    /// Records that a site was reached and returns the failure to inject.
    pub fn check(&self, site: Site) -> Option<Failure> {
        let index = self
            .sites_reached
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let armed = self.fail_nth.load(Ordering::Relaxed);
        if armed != 0 && armed == index {
            return Some(*guard(&self.nth_failure));
        }
        let policy = guard(&self.policies)
            .get(&site)
            .copied()
            .unwrap_or(Policy::Off);
        let mut counts = guard(&self.counts);
        let count = counts.entry(site).or_insert(0);
        *count = count.saturating_add(1);
        let reached = *count;
        drop(counts);
        match policy {
            Policy::Off => None,
            Policy::Always(failure) => Some(failure),
            Policy::Nth(n, failure) if n == reached => Some(failure),
            Policy::Nth(_, _) => None,
            Policy::Chance(numerator, denominator, failure) => {
                if guard(&self.rng).chance(numerator, denominator) {
                    Some(failure)
                } else {
                    None
                }
            }
        }
    }

    /// Returns how many times each site has been reached, for a report.
    pub fn counts(&self) -> BTreeMap<Site, u64> {
        guard(&self.counts).clone()
    }
}

/// Locks a mutex, recovering from poisoning rather than propagating a panic.
fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(inner) => inner,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no policy set, nothing fires, but every call is still counted so a
    /// campaign knows how long the execution was.
    #[test]
    fn nothing_fires_by_default_but_calls_are_counted() {
        let failpoints = Failpoints::new(1);
        for _ in 0..10 {
            assert_eq!(failpoints.check(Site::Write), None);
        }
        assert_eq!(failpoints.sites_reached(), 10);
    }

    /// The nth-call campaign fires exactly once, at the call it names.
    #[test]
    fn the_nth_call_campaign_fires_once_at_the_right_call() {
        for target in 1..=6 {
            let failpoints = Failpoints::new(7);
            failpoints.fail_nth_call(target, Failure::DiskFull);
            let mut fired_at = None;
            for call in 1..=10 {
                if failpoints.check(Site::Read).is_some() {
                    assert!(fired_at.is_none(), "the campaign fired twice");
                    fired_at = Some(call);
                }
            }
            assert_eq!(fired_at, Some(target));
        }
    }

    /// A per-site policy counts that site only.
    #[test]
    fn a_per_site_policy_counts_its_own_site() {
        let failpoints = Failpoints::new(3);
        failpoints.set(Site::Sync, Policy::Nth(2, Failure::IoError));
        assert_eq!(failpoints.check(Site::Write), None);
        assert_eq!(failpoints.check(Site::Sync), None);
        assert_eq!(failpoints.check(Site::Write), None);
        assert_eq!(failpoints.check(Site::Sync), Some(Failure::IoError));
        assert_eq!(failpoints.check(Site::Sync), None);
    }

    /// A chance policy must be reproducible from its seed.
    #[test]
    fn a_chance_policy_replays_from_its_seed() {
        let run = || {
            let failpoints = Failpoints::new(99);
            failpoints.set(Site::Write, Policy::Chance(1, 3, Failure::ShortWrite));
            (0..64)
                .map(|_| failpoints.check(Site::Write).is_some())
                .collect::<Vec<bool>>()
        };
        assert_eq!(run(), run());
        assert!(run().iter().any(|fired| *fired), "the policy never fired");
    }

    /// Every failure that is reported as an error must carry the code its site
    /// would report, so a caller cannot tell an injected failure from a real
    /// one - which is the point.
    #[test]
    fn injected_failures_carry_the_sites_code() {
        for site in Site::all() {
            let error = Failure::IoError
                .to_error(site)
                .expect("an I/O error is reported");
            assert_eq!(error.extended(), site.operation().extended_code());
        }
        assert!(Failure::Crash.to_error(Site::Write).is_none());
        assert!(Failure::ShortWrite.to_error(Site::Write).is_none());
    }
}
