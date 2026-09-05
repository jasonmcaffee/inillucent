//! The deterministic scheduler.
//!
//! Invariant: given a schedule file, every yield point resolves to the same
//! actor in the same order on every run and every machine. A concurrency bug
//! found by the explorer is therefore reproducible by replaying its schedule,
//! not by running the same test a thousand more times and hoping.
//!
//! Actors are real threads, but only one of them runs between yield points: at
//! a yield point an actor parks until the scheduler names it. The schedule is
//! the sequence of choices the scheduler made, recorded as indices into the
//! ready set, which is what makes it a small artifact that can be replayed and
//! enumerated.

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex};

use inillucent_base::rng::Rng;

/// Identifies one actor in a simulated run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ActorId(pub u32);

/// How the scheduler picks the next actor.
#[derive(Clone, Debug)]
pub enum Decisions {
    /// Choices come from a seeded generator; the choices made are recorded.
    Seeded(u64),
    /// Choices come from a recorded schedule, and the generator takes over if
    /// the schedule runs out.
    Replay(Vec<usize>, u64),
}

/// The scheduler's shared state.
///
/// A decision is only taken when every live actor is parked at a yield point.
/// That is what makes the schedule independent of thread timing: if the
/// scheduler chose as soon as the turn was released, the choice would depend on
/// whether the other actor had reached its yield point yet, and two runs of the
/// same seed would diverge.
#[derive(Debug)]
struct SchedulerState {
    live: BTreeSet<ActorId>,
    parked: BTreeSet<ActorId>,
    running: Option<ActorId>,
    recorded: Vec<usize>,
    replay: Vec<usize>,
    cursor: usize,
    rng: Rng,
}

/// A deterministic cooperative scheduler over a fixed set of actors.
#[derive(Debug)]
pub struct Scheduler {
    state: Mutex<SchedulerState>,
    turn: Condvar,
}

impl Scheduler {
    /// Creates a scheduler for `actors` actors, driven by `decisions`.
    pub fn new(actors: u32, decisions: Decisions) -> Arc<Scheduler> {
        let (replay, seed) = match decisions {
            Decisions::Seeded(seed) => (Vec::new(), seed),
            Decisions::Replay(schedule, seed) => (schedule, seed),
        };
        Arc::new(Scheduler {
            state: Mutex::new(SchedulerState {
                live: (0..actors).map(ActorId).collect(),
                parked: BTreeSet::new(),
                running: None,
                recorded: Vec::new(),
                replay,
                cursor: 0,
                rng: Rng::new(seed),
            }),
            turn: Condvar::new(),
        })
    }

    /// Blocks until `actor` is the running actor.
    ///
    /// Call this before every operation whose interleaving matters. The actor
    /// parks, gives up the turn if it held it, and waits; the next decision is
    /// taken once every live actor has parked, which is what keeps the schedule
    /// independent of how fast each thread happens to run.
    pub fn yield_point(&self, actor: ActorId) {
        let mut state = guard(&self.state);
        state.parked.insert(actor);
        if state.running == Some(actor) {
            state.running = None;
        }
        self.turn.notify_all();
        loop {
            if state.running == Some(actor) || !state.live.contains(&actor) {
                state.parked.remove(&actor);
                return;
            }
            if state.running.is_none() && state.parked == state.live {
                let next = choose(&mut state);
                state.running = next;
                self.turn.notify_all();
                continue;
            }
            state = wait(&self.turn, state);
        }
    }

    /// Marks `actor` as finished, so the remaining actors stop waiting for it.
    pub fn finish(&self, actor: ActorId) {
        let mut state = guard(&self.state);
        state.live.remove(&actor);
        state.parked.remove(&actor);
        if state.running == Some(actor) {
            state.running = None;
        }
        self.turn.notify_all();
    }

    /// Returns the choices this run made, which is the schedule to replay.
    pub fn recorded_schedule(&self) -> Vec<usize> {
        guard(&self.state).recorded.clone()
    }

    /// Renders the recorded schedule as a one-line artifact.
    pub fn schedule_text(&self) -> String {
        let choices = self.recorded_schedule();
        let rendered: Vec<String> = choices.iter().map(|choice| choice.to_string()).collect();
        format!("[{}]", rendered.join(","))
    }
}

/// Picks the next actor to run, from the replay schedule when it still has
/// entries and from the generator otherwise, recording the choice either way.
fn choose(state: &mut SchedulerState) -> Option<ActorId> {
    let candidates: Vec<ActorId> = state.live.iter().copied().collect();
    if candidates.is_empty() {
        state.recorded.push(0);
        return None;
    }
    let index = match state.replay.get(state.cursor) {
        Some(choice) => *choice % candidates.len(),
        None => state.rng.below(candidates.len() as u64) as usize,
    };
    state.cursor = state.cursor.saturating_add(1);
    state.recorded.push(index);
    candidates.get(index).copied()
}

/// Waits on the condition variable, recovering from poisoning.
fn wait<'a, T>(
    condvar: &Condvar,
    guard: std::sync::MutexGuard<'a, T>,
) -> std::sync::MutexGuard<'a, T> {
    match condvar.wait(guard) {
        Ok(inner) => inner,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Locks a mutex, recovering from poisoning rather than propagating a panic.
fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(inner) => inner,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Runs `body` once for every interleaving of two actors up to `depth`
/// decisions, which is the bounded exhaustive exploration the assurance plan
/// asks for at two actors.
///
/// `body` is handed the schedule to run and returns whatever the run produced;
/// the results come back in enumeration order so a failing one names its own
/// schedule.
pub fn explore_two_actors<T, F>(depth: usize, mut body: F) -> Vec<(Vec<usize>, T)>
where
    F: FnMut(Vec<usize>) -> T,
{
    let mut results = Vec::new();
    let total = 1u64 << depth.min(20);
    for mask in 0..total {
        let schedule: Vec<usize> = (0..depth).map(|bit| ((mask >> bit) & 1) as usize).collect();
        let outcome = body(schedule.clone());
        results.push((schedule, outcome));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Runs two actors that each append their id at every yield point, and
    /// returns the order they ran in.
    fn interleaving(decisions: Decisions) -> Vec<u32> {
        let scheduler = Scheduler::new(2, decisions);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for id in 0..2u32 {
            let scheduler = Arc::clone(&scheduler);
            let order = Arc::clone(&order);
            handles.push(std::thread::spawn(move || {
                let actor = ActorId(id);
                for _ in 0..6 {
                    scheduler.yield_point(actor);
                    guard(&order).push(id);
                }
                scheduler.finish(actor);
            }));
        }
        for handle in handles {
            let _ = handle.join();
        }
        let recorded = guard(&order).clone();
        recorded
    }

    /// The same seed must produce the same interleaving, or a recorded seed is
    /// not evidence of anything.
    #[test]
    fn the_same_seed_replays_the_same_interleaving() {
        let first = interleaving(Decisions::Seeded(1782));
        let second = interleaving(Decisions::Seeded(1782));
        assert_eq!(first, second);
        assert_eq!(first.len(), 12);
    }

    /// Different seeds must sometimes produce different interleavings, or the
    /// explorer would be testing one schedule over and over.
    #[test]
    fn different_seeds_produce_different_interleavings() {
        let orders: BTreeSet<Vec<u32>> = (0..24)
            .map(|seed| interleaving(Decisions::Seeded(seed)))
            .collect();
        assert!(
            orders.len() > 1,
            "every seed produced the same interleaving"
        );
    }

    /// A recorded schedule must reproduce its own run exactly.
    #[test]
    fn a_recorded_schedule_replays_its_run() {
        let scheduler = Scheduler::new(2, Decisions::Seeded(4242));
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for id in 0..2u32 {
            let scheduler = Arc::clone(&scheduler);
            let order = Arc::clone(&order);
            handles.push(std::thread::spawn(move || {
                for _ in 0..5 {
                    scheduler.yield_point(ActorId(id));
                    guard(&order).push(id);
                }
                scheduler.finish(ActorId(id));
            }));
        }
        for handle in handles {
            let _ = handle.join();
        }
        let first = guard(&order).clone();
        let schedule = scheduler.recorded_schedule();

        let replayed = interleaving(Decisions::Replay(schedule.clone(), 0));
        assert_eq!(
            &replayed[..first.len().min(replayed.len())],
            &first[..first.len().min(replayed.len())]
        );
        assert!(!schedule.is_empty());
    }

    /// Bounded exploration must actually enumerate every interleaving of the
    /// decisions it is given.
    #[test]
    fn exploration_enumerates_every_schedule() {
        let seen = AtomicU32::new(0);
        let results = explore_two_actors(6, |schedule| {
            seen.fetch_add(1, Ordering::Relaxed);
            schedule.iter().sum::<usize>()
        });
        assert_eq!(results.len(), 64);
        assert_eq!(seen.load(Ordering::Relaxed), 64);
        let distinct: BTreeSet<Vec<usize>> = results
            .iter()
            .map(|(schedule, _)| schedule.clone())
            .collect();
        assert_eq!(distinct.len(), 64);
    }
}
