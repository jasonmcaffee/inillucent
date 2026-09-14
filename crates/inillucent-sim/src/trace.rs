//! The event trace a simulated run leaves behind.
//!
//! Invariant: two runs with the same seed, the same schedule, and the same
//! failpoint policy produce byte-identical traces. That is what makes a failure
//! reproducible from a recorded artifact rather than from a description of what
//! someone thinks happened.
//!
//! The trace is newline-delimited JSON, written by hand rather than through a
//! serialisation crate, because the format is part of the evidence contract and
//! a dependency upgrade must not be able to reshape it.

use std::sync::Mutex;

use inillucent_base::checksum;

/// One recorded operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    /// The position of this event in the run, counting from one.
    pub seq: u64,
    /// Which actor performed it.
    pub actor: u32,
    /// The stable name of the yield point or operation.
    pub kind: &'static str,
    /// The file it acted on, when it acted on one.
    pub path: String,
    /// The byte offset it acted at.
    pub offset: u64,
    /// How many bytes it acted on.
    pub length: u64,
    /// What it returned: `ok`, or a code name.
    pub outcome: String,
}

impl Event {
    /// Renders the event as one line of newline-delimited JSON.
    ///
    /// Fields are written in a fixed order so that the trace is diffable and
    /// its digest is stable.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"seq\":{},\"actor\":{},\"kind\":\"{}\",\"path\":\"{}\",\"offset\":{},\"length\":{},\"outcome\":\"{}\"}}",
            self.seq,
            self.actor,
            self.kind,
            escape(&self.path),
            self.offset,
            self.length,
            escape(&self.outcome)
        )
    }
}

/// Escapes the characters JSON forbids in a string body.
///
/// One of three copies until task-1946's M4, and the one that was two branches
/// short: it wrote `\u0008` and `\u000c` where the other two wrote `\b` and
/// `\f`. Both are the same JSON string, so no trace was ever wrong; what the
/// difference cost was a reader deciding whether it was deliberate.
///
/// @param text - the text to escape
fn escape(text: &str) -> String {
    inillucent_base::json::escape(text)
}

/// Every event of one run, in order.
#[derive(Debug, Default)]
pub struct Trace {
    events: Mutex<Vec<Event>>,
}

impl Trace {
    /// Creates an empty trace.
    pub fn new() -> Trace {
        Trace::default()
    }

    /// Appends one event, assigning its sequence number.
    pub fn record(
        &self,
        actor: u32,
        kind: &'static str,
        path: &str,
        offset: u64,
        length: u64,
        outcome: &str,
    ) {
        let mut events = guard(&self.events);
        let seq = events.len() as u64 + 1;
        events.push(Event {
            seq,
            actor,
            kind,
            path: path.to_string(),
            offset,
            length,
            outcome: outcome.to_string(),
        });
    }

    /// Returns how many events have been recorded.
    pub fn len(&self) -> usize {
        guard(&self.events).len()
    }

    /// Reports whether the trace is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a copy of the events.
    pub fn events(&self) -> Vec<Event> {
        guard(&self.events).clone()
    }

    /// Renders the whole trace as newline-delimited JSON.
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for event in guard(&self.events).iter() {
            out.push_str(&event.to_json());
            out.push('\n');
        }
        out
    }

    /// Returns a digest of the trace, which is what two runs are compared by.
    pub fn digest(&self) -> u32 {
        checksum::crc32(self.to_jsonl().as_bytes())
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

    /// A trace must serialise in a stable order with stable field names, or a
    /// recorded artifact stops matching the run it came from.
    #[test]
    fn events_serialise_in_a_fixed_shape() {
        let trace = Trace::new();
        trace.record(0, "write", "app.db", 4096, 512, "ok");
        assert_eq!(
            trace.to_jsonl().trim(),
            "{\"seq\":1,\"actor\":0,\"kind\":\"write\",\"path\":\"app.db\",\"offset\":4096,\"length\":512,\"outcome\":\"ok\"}"
        );
    }

    /// Paths with characters JSON cannot hold must be escaped rather than
    /// producing an unparseable line.
    #[test]
    fn awkward_paths_are_escaped() {
        let trace = Trace::new();
        trace.record(1, "open", "C:\\data\\\"odd\"\nname", 0, 0, "ok");
        let line = trace.to_jsonl();
        assert!(line.contains("C:\\\\data\\\\\\\"odd\\\"\\nname"), "{line}");
        assert_eq!(line.lines().count(), 1);
    }

    /// The digest must change when the run changes and match when it does not.
    #[test]
    fn the_digest_follows_the_events() {
        let first = Trace::new();
        let second = Trace::new();
        for trace in [&first, &second] {
            trace.record(0, "read", "a.db", 0, 16, "ok");
            trace.record(1, "read", "a.db", 16, 16, "ok");
        }
        assert_eq!(first.digest(), second.digest());
        second.record(1, "sync", "a.db", 0, 0, "ok");
        assert_ne!(first.digest(), second.digest());
    }
}
