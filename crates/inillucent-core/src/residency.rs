//! When the embedding model is in memory.
//!
//! Invariant: **a policy this holds is a policy the process keeps, and the
//! counters say so.** `on-demand` drops the session before the call returns;
//! `idle` drops it after its timer and not before; `resident` never drops it.
//! Each of those is asserted by a test that reads the load and eviction counts
//! rather than by inspecting memory, because a heap that has not shrunk is not
//! evidence that a session is still open and a session that is still open is
//! exactly what the test is about.
//!
//! ## Why there is a choice at all
//!
//! Opening a session on `nomic-embed-text-v1.5` costs **650 to 800 ms** on this
//! machine, and an embedding through an already-open one costs **12 to 36 ms**.
//! `docs/embeddings.md` has the whole table and
//! `inillucent-bench embed-residency` is the command that produced it. Nothing
//! available moves the load much: turning every graph optimization off saves
//! about 15%, and so does loading a pre-optimized graph. The load is reading and
//! materializing half a gigabyte of weights.
//!
//! So the model held in memory is worth about a factor of thirty on a query, and
//! costs about 1.9 GB of a machine. Which of those matters depends entirely on
//! what the process is:
//!
//! | | what it is for |
//! |---|---|
//! | [`Residency::Resident`] | an ingestion run, or a server that searches constantly |
//! | [`Residency::OnDemand`] | a process that answers one question and exits - an agent's MCP transport is exactly this |
//! | [`Residency::Idle`] | a person asking questions: the load is paid once for a burst, and the memory comes back afterwards |
//!
//! `Idle` is the default for the reason the third row gives.

use std::time::Duration;

use anyhow::{Context, Result};

// The manager needs a session type, which only exists when the runtime is
// linked. The policy does not: `inillucent setup-embeddings` writes a profile
// into the install state on a build that has never seen ONNX Runtime, and the
// two have to agree on what the words mean.
#[cfg(feature = "onnx")]
use std::path::{Path, PathBuf};
#[cfg(feature = "onnx")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "onnx")]
use std::sync::{Arc, Condvar, Mutex, Weak};
#[cfg(feature = "onnx")]
use std::time::Instant;

#[cfg(feature = "onnx")]
use crate::embed::Embedder;
#[cfg(feature = "onnx")]
use crate::embed_onnx::{OnnxEmbedder, OnnxOptions};

/// How long an `idle` profile keeps the model with nothing asking for it.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(300);

/// When the model is in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Residency {
    /// Loaded on first use and kept for the life of the process.
    Resident,
    /// Loaded for the call and dropped before it returns.
    OnDemand,
    /// Loaded on use, dropped after this long with nothing asking for it.
    Idle(Duration),
}

impl Default for Residency {
    fn default() -> Self {
        Residency::Idle(DEFAULT_IDLE)
    }
}

impl Residency {
    /// Parses a profile as it is written on a command line or in the state file.
    ///
    /// `idle` takes an optional duration - `idle:90s`, `idle:5m`, `idle:2h` -
    /// because five minutes is a default rather than a law, and a caller that
    /// wants ninety seconds should not have to choose between the two profiles
    /// on either side of it.
    ///
    /// @param text - `resident`, `on-demand`, `idle`, or `idle:<duration>`
    pub fn parse(text: &str) -> Result<Residency> {
        let text = text.trim().to_ascii_lowercase();
        match text.as_str() {
            "resident" | "always" => return Ok(Residency::Resident),
            "on-demand" | "ondemand" | "per-call" | "never" => return Ok(Residency::OnDemand),
            "idle" => return Ok(Residency::Idle(DEFAULT_IDLE)),
            _ => {}
        }
        if let Some(rest) = text.strip_prefix("idle:") {
            return Ok(Residency::Idle(parse_duration(rest)?));
        }
        anyhow::bail!(
            "unknown residency profile {text}, expected resident, on-demand, idle or idle:<duration>"
        )
    }

    /// The profile as it is written back into the state file.
    pub fn label(&self) -> String {
        match self {
            Residency::Resident => "resident".to_string(),
            Residency::OnDemand => "on-demand".to_string(),
            Residency::Idle(after) => format!("idle:{}s", after.as_secs()),
        }
    }

    /// The profile this process should use.
    ///
    /// The environment first, then what the install recorded, then the default.
    /// The environment first because one process wanting a different answer from
    /// the machine's is the common case and it should not have to rewrite a file
    /// to get it.
    ///
    /// A value that does not parse is reported and then ignored, rather than
    /// stopping the process: a typo in a variable should not take a database
    /// down, and a silent fallback with nothing on standard error is how a
    /// machine runs for a month on a profile nobody chose.
    pub fn configured() -> Residency {
        if let Ok(named) = std::env::var(crate::install::RESIDENCY_VAR) {
            if !named.trim().is_empty() {
                match Residency::parse(&named) {
                    Ok(policy) => return policy,
                    Err(reason) => eprintln!(
                        "inillucent: ignoring {}: {reason}",
                        crate::install::RESIDENCY_VAR
                    ),
                }
            }
        }
        let recorded = crate::install::read_state(&crate::install::home())
            .and_then(|state| state.residency)
            .and_then(|text| Residency::parse(&text).ok());
        recorded.unwrap_or_default()
    }
}

/// Parses `90s`, `5m`, `2h`, or a bare number of seconds.
///
/// @param text - the duration
fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let (digits, scale) = match text.chars().last() {
        Some('s') => (&text[..text.len() - 1], 1),
        Some('m') => (&text[..text.len() - 1], 60),
        Some('h') => (&text[..text.len() - 1], 3_600),
        _ => (text, 1),
    };
    let value: u64 = digits
        .trim()
        .parse()
        .with_context(|| format!("{text} is not a duration; write 90s, 5m or 2h"))?;
    anyhow::ensure!(
        value > 0,
        "an idle timeout of zero is on-demand; write on-demand"
    );
    Ok(Duration::from_secs(value.saturating_mul(scale)))
}

/// What a managed embedder has done, for a caller deciding whether it chose the
/// right profile.
///
/// A profile whose numbers say it loaded the model four hundred times is a
/// profile chosen wrongly, and that has to be visible without a profiler.
#[cfg(feature = "onnx")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Sessions opened.
    pub loads: u64,
    /// Sessions dropped because the policy said to.
    pub evictions: u64,
    /// Texts embedded.
    pub embeddings: u64,
    /// Milliseconds spent opening sessions.
    pub load_ms: u64,
    /// Milliseconds spent inside the model.
    pub embed_ms: u64,
}

/// The parts a reaper thread has to reach without keeping the model alive.
#[cfg(feature = "onnx")]
struct Shared {
    dir: PathBuf,
    model_file: String,
    options: OnnxOptions,
    residency: Residency,
    /// The session, and when it was last used.
    ///
    /// One `Mutex` over both, because "is it loaded" and "when was it last
    /// touched" are read together by the reaper and written together by a call,
    /// and two locks over the two halves is how a reaper evicts a session that a
    /// call had just claimed.
    state: Mutex<Loaded>,
    /// Woken when a call finishes, so a reaper that is parked until a deadline
    /// that has just moved does not have to sleep through the old one.
    idle: Condvar,
    loads: AtomicU64,
    evictions: AtomicU64,
    embeddings: AtomicU64,
    load_ms: AtomicU64,
    embed_ms: AtomicU64,
}

/// The session, when there is one, and when it was last used.
#[cfg(feature = "onnx")]
struct Loaded {
    embedder: Option<OnnxEmbedder>,
    last_used: Instant,
    /// Whether a reaper thread is already parked on this state.
    reaping: bool,
}

/// An embedder that loads and unloads according to a policy.
///
/// Cheap to clone: every clone names the same session and the same counters, so
/// a server can hand one to each of its request paths without any of them
/// loading a second copy of the weights.
#[cfg(feature = "onnx")]
#[derive(Clone)]
pub struct ManagedEmbedder {
    shared: Arc<Shared>,
}

#[cfg(feature = "onnx")]
impl ManagedEmbedder {
    /// Builds one over a model directory. Nothing is loaded until the first call.
    ///
    /// Deliberately lazy even for [`Residency::Resident`]: a process that builds
    /// a connection and never embeds anything should not pay 800 ms and 1.9 GB
    /// for a function it did not call. `resident` is a statement about what
    /// happens after the first use, not about start-up.
    ///
    /// @param dir - the model directory
    /// @param model_file - the weights file inside it
    /// @param options - how the session is opened
    /// @param residency - when the model is in memory
    pub fn new(
        dir: impl AsRef<Path>,
        model_file: impl Into<String>,
        options: OnnxOptions,
        residency: Residency,
    ) -> ManagedEmbedder {
        ManagedEmbedder {
            shared: Arc::new(Shared {
                dir: dir.as_ref().to_path_buf(),
                model_file: model_file.into(),
                options,
                residency,
                state: Mutex::new(Loaded {
                    embedder: None,
                    last_used: Instant::now(),
                    reaping: false,
                }),
                idle: Condvar::new(),
                loads: AtomicU64::new(0),
                evictions: AtomicU64::new(0),
                embeddings: AtomicU64::new(0),
                load_ms: AtomicU64::new(0),
                embed_ms: AtomicU64::new(0),
            }),
        }
    }

    /// The policy in force.
    pub fn residency(&self) -> Residency {
        self.shared.residency
    }

    /// Whether the weights are in memory right now.
    pub fn loaded(&self) -> bool {
        self.shared
            .state
            .lock()
            .map(|state| state.embedder.is_some())
            .unwrap_or(false)
    }

    /// What this embedder has done so far.
    pub fn stats(&self) -> Stats {
        Stats {
            loads: self.shared.loads.load(Ordering::Relaxed),
            evictions: self.shared.evictions.load(Ordering::Relaxed),
            embeddings: self.shared.embeddings.load(Ordering::Relaxed),
            load_ms: self.shared.load_ms.load(Ordering::Relaxed),
            embed_ms: self.shared.embed_ms.load(Ordering::Relaxed),
        }
    }

    /// Drops the session now, whatever the policy says.
    ///
    /// For a caller that knows it has finished - an ingestion run that has
    /// embedded its last chunk - and would rather not wait out an idle timer.
    pub fn unload(&self) {
        let Ok(mut state) = self.shared.state.lock() else {
            return;
        };
        if state.embedder.take().is_some() {
            self.shared.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Embeds texts as documents, applying the model's document prefix.
    ///
    /// @param texts - the texts
    pub fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.with_session(texts.len() as u64, |embedder| {
            embedder.embed_documents(texts)
        })
    }

    /// Embeds one text as a query, applying the model's query prefix.
    ///
    /// @param text - the query
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.with_session(1, |embedder| embedder.embed_query(text))
    }

    /// Embeds texts that already carry whatever prefix they need.
    ///
    /// @param texts - the prefixed texts
    pub fn embed_prefixed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.with_session(texts.len() as u64, |embedder| {
            embedder.embed_prefixed(texts)
        })
    }

    /// Runs one operation against a loaded session, loading and unloading as the
    /// policy requires.
    ///
    /// The lock is held across the inference call. One ONNX session cannot be run
    /// from two threads at once in any case, and the alternative - releasing the
    /// lock and holding a reference to the session - is what would let a reaper
    /// drop the weights out from under a call in flight.
    ///
    /// @param texts - how many texts this call is embedding, for the counters
    /// @param run - what to do with the session
    fn with_session<T>(
        &self,
        texts: u64,
        run: impl FnOnce(&OnnxEmbedder) -> Result<T>,
    ) -> Result<T> {
        let shared = &self.shared;
        let mut state = shared
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("the embedder's lock was poisoned by an earlier panic"))?;

        if state.embedder.is_none() {
            let started = Instant::now();
            let embedder =
                OnnxEmbedder::open_model(&shared.dir, &shared.model_file, shared.options.clone())
                    .with_context(|| {
                    format!(
                        "loading {} from {} for the {} profile",
                        shared.model_file,
                        shared.dir.display(),
                        shared.residency.label()
                    )
                })?;
            shared
                .load_ms
                .fetch_add(elapsed_ms(started), Ordering::Relaxed);
            shared.loads.fetch_add(1, Ordering::Relaxed);
            state.embedder = Some(embedder);
        }

        let started = Instant::now();
        let outcome = {
            let Some(embedder) = state.embedder.as_ref() else {
                return Err(anyhow::anyhow!(
                    "the embedder vanished between loading and running"
                ));
            };
            run(embedder)
        };
        shared
            .embed_ms
            .fetch_add(elapsed_ms(started), Ordering::Relaxed);
        shared.embeddings.fetch_add(texts, Ordering::Relaxed);
        state.last_used = Instant::now();

        match shared.residency {
            Residency::OnDemand => {
                // Dropped before the call returns, and before the lock is
                // released, so "on-demand held nothing between calls" is true
                // rather than eventually true.
                state.embedder = None;
                shared.evictions.fetch_add(1, Ordering::Relaxed);
            }
            Residency::Idle(after) => {
                if !state.reaping {
                    state.reaping = true;
                    spawn_reaper(Arc::downgrade(shared), after);
                }
                shared.idle.notify_all();
            }
            Residency::Resident => {}
        }

        outcome
    }
}

/// Milliseconds since an instant, saturating rather than wrapping.
///
/// @param started - when the span began
#[cfg(feature = "onnx")]
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Starts the thread that drops an idle session.
///
/// It holds a [`Weak`], not an `Arc`, for two reasons. Dropping the last
/// `ManagedEmbedder` then drops the session immediately rather than at the next
/// deadline, which is what a short-lived process needs. And a process that
/// creates and discards embedders does not accumulate threads each keeping half
/// a gigabyte of weights alive until a timer nobody is waiting for expires.
///
/// @param shared - a weak handle to the state
/// @param after - how long with no call before the session is dropped
#[cfg(feature = "onnx")]
fn spawn_reaper(shared: Weak<Shared>, after: Duration) {
    let handed = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("inillucent-embed-reaper".to_string())
        .spawn(move || reap(handed, after));
    if let Err(error) = spawned {
        // A machine that cannot start a thread is a machine in trouble, and the
        // right behaviour is to keep answering queries with the model resident
        // rather than to fail the query that happened to be first. The flag is
        // cleared so a later call tries again.
        eprintln!("inillucent: no idle reaper for the embedder ({error}); it stays resident");
        if let Some(shared) = shared.upgrade() {
            if let Ok(mut state) = shared.state.lock() {
                state.reaping = false;
            }
        }
    }
}

/// Waits out the idle period and drops the session if nothing used it.
///
/// @param shared - a weak handle to the state
/// @param after - the idle period
#[cfg(feature = "onnx")]
fn reap(shared: Weak<Shared>, after: Duration) {
    loop {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let Ok(state) = shared.state.lock() else {
            return;
        };
        let waited = shared.idle.wait_timeout(state, after);
        let Ok((mut state, _)) = waited else { return };
        if state.embedder.is_none() {
            // Somebody unloaded it by hand, or the policy changed underneath.
            // Either way there is nothing left to reap and no reason to keep a
            // thread parked; the next call starts a new one.
            state.reaping = false;
            return;
        }
        if state.last_used.elapsed() >= after {
            state.embedder = None;
            shared.evictions.fetch_add(1, Ordering::Relaxed);
            state.reaping = false;
            return;
        }
        // Used while this thread was parked, so the deadline has moved. Round
        // again rather than dropping a session somebody is using.
        drop(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every form a profile can be written in parses to the policy it names.
    #[test]
    fn a_profile_parses_from_every_form_it_is_written_in() {
        assert_eq!(Residency::parse("resident").unwrap(), Residency::Resident);
        assert_eq!(Residency::parse(" RESIDENT ").unwrap(), Residency::Resident);
        assert_eq!(Residency::parse("on-demand").unwrap(), Residency::OnDemand);
        assert_eq!(Residency::parse("ondemand").unwrap(), Residency::OnDemand);
        assert_eq!(
            Residency::parse("idle").unwrap(),
            Residency::Idle(DEFAULT_IDLE)
        );
        assert_eq!(
            Residency::parse("idle:90s").unwrap(),
            Residency::Idle(Duration::from_secs(90))
        );
        assert_eq!(
            Residency::parse("idle:5m").unwrap(),
            Residency::Idle(Duration::from_secs(300))
        );
        assert_eq!(
            Residency::parse("idle:2h").unwrap(),
            Residency::Idle(Duration::from_secs(7_200))
        );
        assert_eq!(
            Residency::parse("idle:45").unwrap(),
            Residency::Idle(Duration::from_secs(45))
        );
    }

    /// A profile that names nothing this build knows is refused rather than
    /// falling back, because a silent fallback is a machine running for a month
    /// on a profile nobody chose.
    #[test]
    fn an_unknown_profile_is_refused() {
        assert!(Residency::parse("sometimes").is_err());
        assert!(Residency::parse("idle:soon").is_err());
        assert!(Residency::parse("idle:0").is_err());
        assert!(Residency::parse("").is_err());
    }

    /// A profile survives being written and read back, which is what the state
    /// file does with it.
    #[test]
    fn a_profile_round_trips_through_its_own_label() {
        for policy in [
            Residency::Resident,
            Residency::OnDemand,
            Residency::Idle(DEFAULT_IDLE),
            Residency::Idle(Duration::from_secs(90)),
        ] {
            assert_eq!(Residency::parse(&policy.label()).unwrap(), policy);
        }
    }

    /// The default is `idle`, and the reason is in this module's own header.
    #[test]
    fn the_default_profile_is_idle() {
        assert_eq!(Residency::default(), Residency::Idle(DEFAULT_IDLE));
    }

    /// Builds a managed embedder over whatever model this machine has, or
    /// reports that it has none and lets the caller skip.
    ///
    /// A skip rather than a failure because these tests are about a policy and
    /// not about a model: a machine with no weights should say so once, not fail
    /// four tests for a reason none of them is checking.
    ///
    /// @param residency - the policy under test
    #[cfg(feature = "onnx")]
    fn managed_or_skip(residency: Residency) -> Option<ManagedEmbedder> {
        // Resolving the model reads `INILLUCENT_ONNX_DIR`, which `install`'s own
        // cases point at directories they made. The lock is held only for the
        // lookup, because the embedding that follows takes seconds and holding
        // it there would serialize four cases that have no reason to wait.
        let dir = {
            let _held = crate::install::env_guard();
            crate::install::model_dir(crate::install::DEFAULT_MODEL)?
        };
        let manifest = crate::model::ModelManifest::read(&dir)
            .unwrap_or_else(|_| crate::model::ModelManifest::nomic_v1_5());
        let options = OnnxOptions {
            batch_size: 1,
            ..OnnxOptions::for_model(&manifest)
        };
        let managed = ManagedEmbedder::new(&dir, manifest.model_file.clone(), options, residency);
        // One embedding up front, so a machine that has the weights but cannot
        // load the runtime skips rather than failing inside the assertions.
        if managed.embed_query("a warm up").is_err() {
            eprintln!("skipping: the ONNX runtime would not load");
            return None;
        }
        Some(managed)
    }

    /// `resident` loads once however many times it is called.
    #[cfg(feature = "onnx")]
    #[test]
    fn resident_loads_once_and_never_evicts() {
        let Some(managed) = managed_or_skip(Residency::Resident) else {
            return;
        };
        for _ in 0..3 {
            managed.embed_query("another question").unwrap();
        }
        let stats = managed.stats();
        assert_eq!(stats.loads, 1, "one load for four calls");
        assert_eq!(stats.evictions, 0);
        assert!(managed.loaded(), "resident holds the session");
    }

    /// `on-demand` loads for every call and holds nothing between them.
    ///
    /// Counted rather than measured: a heap that has not shrunk is not evidence
    /// that a session is open, and a session being open is what this is about.
    #[cfg(feature = "onnx")]
    #[test]
    fn on_demand_loads_per_call_and_holds_nothing_afterwards() {
        let Some(managed) = managed_or_skip(Residency::OnDemand) else {
            return;
        };
        assert!(!managed.loaded(), "nothing is held after the warm up call");
        managed.embed_query("a second question").unwrap();
        managed.embed_query("a third question").unwrap();
        let stats = managed.stats();
        assert_eq!(stats.loads, 3, "one load per call, including the warm up");
        assert_eq!(stats.evictions, 3);
        assert!(!managed.loaded());
    }

    /// `idle` keeps the session for a burst and drops it after the timer.
    #[cfg(feature = "onnx")]
    #[test]
    fn idle_holds_through_a_burst_and_drops_afterwards() {
        let Some(managed) = managed_or_skip(Residency::Idle(Duration::from_millis(400))) else {
            return;
        };
        managed.embed_query("straight after").unwrap();
        assert_eq!(managed.stats().loads, 1, "a burst pays the load once");
        assert!(managed.loaded());

        // Twice the idle period plus the reaper's own wake-up, which is bounded
        // by the same period. Sleeping rather than signalling because the thing
        // under test is a timer.
        let deadline = Instant::now() + Duration::from_secs(10);
        while managed.loaded() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!managed.loaded(), "the idle reaper dropped the session");
        assert_eq!(managed.stats().evictions, 1);

        managed.embed_query("much later").unwrap();
        assert_eq!(managed.stats().loads, 2, "the next call loads it again");
    }

    /// `unload` drops the session whatever the policy says.
    #[cfg(feature = "onnx")]
    #[test]
    fn unloading_by_hand_drops_a_resident_session() {
        let Some(managed) = managed_or_skip(Residency::Resident) else {
            return;
        };
        assert!(managed.loaded());
        managed.unload();
        assert!(!managed.loaded());
        assert_eq!(managed.stats().evictions, 1);
        managed.embed_query("after the unload").unwrap();
        assert_eq!(managed.stats().loads, 2);
    }

    /// Two clones name one session, so a server handing one to each request path
    /// does not load a second copy of the weights.
    #[cfg(feature = "onnx")]
    #[test]
    fn a_clone_shares_the_session_rather_than_loading_another() {
        let Some(managed) = managed_or_skip(Residency::Resident) else {
            return;
        };
        let second = managed.clone();
        second.embed_query("through the clone").unwrap();
        assert_eq!(managed.stats().loads, 1);
        assert_eq!(second.stats().embeddings, managed.stats().embeddings);
    }
}
