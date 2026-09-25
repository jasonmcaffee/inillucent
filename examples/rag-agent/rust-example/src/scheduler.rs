//! Runs a sync at start, then on a timer, and whenever a tool asks for one.
//!
//! The sync runs on a thread of its own so the MCP loop keeps answering while
//! documents are embedded. The two threads share the database through
//! `SharedDatabase`, which runs one statement at a time, and they share the
//! sync's progress through [`SyncState`].
//!
//! Only one sync runs at a time. A request that arrives during a sync is
//! answered with that sync's progress and starts nothing new, because a second
//! pass over the same source would find nothing the first did not.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::clock::{format_utc, utc_now};
use crate::config::IndexSettings;
use crate::store::Store;
use crate::sync::{run_sync, Progress, SyncReport};

/// What the sync thread shares with the tools.
#[derive(Default)]
pub struct SyncState {
    /// Set while a sync runs.
    running: AtomicBool,
    /// What the running sync is doing.
    pub progress: Mutex<Progress>,
    /// The last sync that finished.
    pub last_report: Mutex<Option<SyncReport>>,
    /// The last sync that failed before it could finish, and why.
    pub last_error: Mutex<Option<String>>,
    /// When the timer starts the next sync.
    pub next_due: Mutex<Option<String>>,
}

/// The handle the MCP tools use to read the sync's state and to start one.
#[derive(Clone)]
pub struct Scheduler {
    state: Arc<SyncState>,
    trigger: Sender<()>,
    interval: Option<Duration>,
}

impl Scheduler {
    /// Starts the sync thread. It runs the first sync straight away.
    ///
    /// @param store - the database
    /// @param source - the JSONL file or folder to sync from
    /// @param settings - the chunking and context settings
    /// @param interval - how long to wait between syncs, or nothing for no timer
    pub fn start(store: Store, source: PathBuf, settings: IndexSettings, interval: Option<Duration>) -> Scheduler {
        let state = Arc::new(SyncState::default());
        let (trigger, requests) = channel::<()>();
        let shared = Arc::clone(&state);
        std::thread::spawn(move || loop {
            run_once(&store, &source, &settings, &shared);
            *lock(&shared.next_due) = interval.map(due_after);
            let waited = match interval {
                Some(interval) => requests.recv_timeout(interval),
                None => requests.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            if waited == Err(RecvTimeoutError::Disconnected) {
                break;
            }
        });
        Scheduler { state, trigger, interval }
    }

    /// Asks the sync thread to run a sync now.
    ///
    /// Returns `false` when a sync is already running.
    pub fn request_sync(&self) -> bool {
        if self.state.running.load(Ordering::SeqCst) {
            return false;
        }
        self.trigger.send(()).is_ok()
    }

    /// Returns the running sync's progress.
    pub fn progress(&self) -> Progress {
        lock(&self.state.progress).clone()
    }

    /// Returns everything `sync_status` reports.
    pub fn status(&self) -> serde_json::Value {
        json!({
            "progress": self.progress(),
            "last_report": *lock(&self.state.last_report),
            "last_error": *lock(&self.state.last_error),
            "next_sync_due": *lock(&self.state.next_due),
            "interval_seconds": self.interval.map(|interval| interval.as_secs()),
        })
    }
}

/// Runs one sync and records how it ended.
///
/// @param store - the database
/// @param source - the JSONL file or folder
/// @param settings - the chunking and context settings
/// @param state - where the result goes
fn run_once(store: &Store, source: &std::path::Path, settings: &IndexSettings, state: &SyncState) {
    state.running.store(true, Ordering::SeqCst);
    *lock(&state.progress) = Progress { running: true, started_at: Some(utc_now()), ..Progress::default() };
    match run_sync(store, source, settings, &state.progress) {
        Ok(report) => {
            eprintln!(
                "sync: {} added, {} updated, {} removed, {} unchanged, {} chunks in {:.1} s",
                report.added.len(),
                report.updated.len(),
                report.removed.len(),
                report.unchanged,
                report.chunks_written,
                report.total_seconds
            );
            *lock(&state.last_report) = Some(report);
            *lock(&state.last_error) = None;
        }
        Err(error) => {
            eprintln!("sync failed: {error}");
            *lock(&state.last_error) = Some(error);
        }
    }
    *lock(&state.progress) = Progress::default();
    state.running.store(false, Ordering::SeqCst);
}

/// Returns the time an interval from now, as a timestamp.
///
/// @param interval - how long from now
fn due_after(interval: Duration) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format_utc((now + interval).as_secs())
}

/// Locks a mutex, and still returns the value if another thread panicked while holding it.
///
/// @param mutex - the mutex
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
