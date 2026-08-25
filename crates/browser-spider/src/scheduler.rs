//! [`Scheduler`]: a `BinaryHeap<Reverse<QueueKey>>` plus pending mirror, seen set,
//! ticket-based in-flight map and snapshot/restore.
//!
//! Python builds this out of an `asyncio.PriorityQueue` of `(-priority, counter, request)`
//! tuples, a `set` of fingerprints, a `_pending` dict mirroring the queue (so a checkpoint
//! can be taken without draining it) and an `_inflight` dict keyed by `id(request)`.
//!
//! The port keeps all four, with one deliberate change: `id(request)` has no Rust
//! analogue, so [`Scheduler::dequeue`] hands back a [`Ticket`] alongside the request and
//! [`Scheduler::complete`] takes that ticket. This is behaviourally identical — Python only
//! ever calls `complete()` with the request the task wrapper just dequeued — and it fixes
//! the one place where Python's identity keying is fragile: the same `Request` object
//! enqueued twice with `dont_filter=True` needs a `list` of counters and LIFO pops there,
//! whereas a ticket is unique by construction.
//!
//! # Ordering
//!
//! Higher `priority` is dequeued first; ties break FIFO on an insertion counter. The heap
//! is a max-heap, so it stores `Reverse(QueueKey { neg_priority, counter })` and the
//! smallest `(-priority, counter)` pops first — exactly Python's tuple ordering.
//!
//! ```
//! use browser_spider::request::{FingerprintOptions, Request};
//! use browser_spider::scheduler::Scheduler;
//!
//! let mut scheduler = Scheduler::new(FingerprintOptions::default());
//! assert!(scheduler.enqueue(Request::new("http://example.com/a")));
//! assert!(scheduler.enqueue(Request::new("http://example.com/b").with_priority(10)));
//! // The duplicate is dropped.
//! assert!(!scheduler.enqueue(Request::new("http://example.com/a")));
//!
//! let (ticket, request) = scheduler.dequeue().expect("queued");
//! assert_eq!(request.url, "http://example.com/b"); // priority 10 first
//! scheduler.complete(ticket);
//! ```

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::checkpoint::CheckpointData;
use crate::request::{Fingerprint, FingerprintOptions, Request};

/// The heap ordering key: Python's `(-priority, counter)` tuple.
///
/// Derived `Ord` compares `neg_priority` first and `counter` second, which is the field
/// order — do not reorder the fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct QueueKey {
    /// `-request.priority`, so a *lower* key means a *higher* priority.
    ///
    /// Widened to `i64` on purpose: `-(i32::MIN)` does not fit in an `i32`, and a
    /// saturating negation would make `priority = i32::MIN` and `priority = i32::MIN + 1`
    /// tie — Python, whose ints are unbounded, orders them apart.
    pub neg_priority: i64,
    /// Monotonic insertion counter, breaking priority ties FIFO.
    pub counter: u64,
}

/// A handle to one dequeued-but-unfinished request.
///
/// Hand it back to [`Scheduler::complete`] when the request is done — successfully or
/// not — so it stops being included in checkpoints. Replaces Python's `id(request)`
/// bookkeeping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ticket(u64);

impl Ticket {
    /// The insertion counter behind this ticket. Useful for tracing; carries no meaning
    /// beyond ordering.
    pub fn counter(self) -> u64 {
        self.0
    }
}

/// A priority queue of [`Request`]s with fingerprint deduplication.
///
/// Not internally synchronised: the engine owns one and drives it from the crawl loop
/// (behind a `Mutex` when worker tasks need to enqueue).
#[derive(Debug)]
pub struct Scheduler {
    heap: BinaryHeap<Reverse<QueueKey>>,
    /// Python's `_pending`: counter → the enqueued priority and the request, kept so
    /// `snapshot()` never has to drain the queue. Entries survive `dequeue` and are only
    /// removed by `complete`, which is what makes an in-flight request checkpointable.
    pending: HashMap<u64, (i64, Request)>,
    seen: HashSet<Fingerprint>,
    inflight: HashSet<u64>,
    counter: u64,
    options: FingerprintOptions,
}

impl Scheduler {
    /// A scheduler that fingerprints with `options`.
    pub fn new(options: FingerprintOptions) -> Self {
        Self {
            heap: BinaryHeap::new(),
            pending: HashMap::new(),
            seen: HashSet::new(),
            inflight: HashSet::new(),
            counter: 0,
            options,
        }
    }

    /// The fingerprint options this scheduler was built with.
    pub fn options(&self) -> FingerprintOptions {
        self.options
    }

    /// Add a request. Returns `false` when it was dropped as a duplicate.
    ///
    /// The fingerprint is recorded as seen **even when `dont_filter` is set**, so a later
    /// filtered request with the same fingerprint is still dropped — Python's behaviour,
    /// and the reason a retry must set `dont_filter`.
    pub fn enqueue(&mut self, request: Request) -> bool {
        let fingerprint = request.update_fingerprint(self.options);

        if !request.dont_filter && self.seen.contains(&fingerprint) {
            tracing::debug!("Dropped duplicate request: {request}");
            return false;
        }
        self.seen.insert(fingerprint);

        let counter = self.next_counter();
        let neg_priority = -i64::from(request.priority);
        self.pending.insert(counter, (neg_priority, request));
        self.heap.push(Reverse(QueueKey {
            neg_priority,
            counter,
        }));
        true
    }

    /// Take the highest-priority request, if any.
    ///
    /// Python's version awaits an empty queue; the port returns `None` instead and lets
    /// the engine decide whether to idle, checkpoint or finish — the completion condition
    /// ("queue empty **and** nothing in flight") is the engine's, not the scheduler's.
    ///
    /// The request stays in the pending mirror until [`Scheduler::complete`] is called
    /// with the returned ticket, so a crash or a pause mid-flight re-fetches it on resume.
    pub fn dequeue(&mut self) -> Option<(Ticket, Request)> {
        let Reverse(key) = self.heap.pop()?;
        let (_, request) = self
            .pending
            .get(&key.counter)
            .expect("every heap entry has a pending mirror entry");
        self.inflight.insert(key.counter);
        Some((Ticket(key.counter), request.clone()))
    }

    /// Mark a dequeued request as finished.
    ///
    /// Idempotent, and a no-op for a ticket that was never handed out or has already been
    /// completed.
    pub fn complete(&mut self, ticket: Ticket) {
        if self.inflight.remove(&ticket.0) {
            self.pending.remove(&ticket.0);
        }
    }

    /// The number of *queued* requests.
    ///
    /// Python's `__len__` is `queue.qsize()`: dequeued-but-uncompleted requests are **not**
    /// counted here even though they do appear in [`Scheduler::snapshot`].
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether the queue is empty. In-flight requests do not count — see [`Scheduler::len`].
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// How many requests are dequeued but not yet completed.
    pub fn inflight_len(&self) -> usize {
        self.inflight.len()
    }

    /// How many requests a checkpoint taken right now would carry (queued + in flight).
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// How many distinct fingerprints have been seen.
    pub fn seen_len(&self) -> usize {
        self.seen.len()
    }

    /// Whether this fingerprint has been enqueued before.
    pub fn has_seen(&self, fingerprint: &Fingerprint) -> bool {
        self.seen.contains(fingerprint)
    }

    /// The checkpointable state: every queued **and** in-flight request in queue order,
    /// plus a copy of the seen set.
    ///
    /// The returned values are copies; mutating them does not touch the scheduler.
    pub fn snapshot(&self) -> (Vec<Request>, HashSet<Fingerprint>) {
        let mut entries: Vec<(&u64, &(i64, Request))> = self.pending.iter().collect();
        // Python sorts by the tuple `(neg_priority, counter)` recorded at enqueue time,
        // not by the request's current priority — a retry that mutates `priority` after
        // enqueue keeps its original queue position.
        entries.sort_by_key(|(counter, (neg_priority, _))| (*neg_priority, **counter));
        (
            entries
                .into_iter()
                .map(|(_, (_, request))| request.clone())
                .collect(),
            self.seen.clone(),
        )
    }

    /// Reload a checkpoint: replace the seen set and re-queue every saved request.
    ///
    /// The requests arrive already in queue order, so they are re-numbered with fresh
    /// counters and keep that order among equal priorities.
    pub fn restore(&mut self, data: &CheckpointData) {
        self.seen.clone_from(&data.seen);

        for request in &data.requests {
            let counter = self.next_counter();
            let neg_priority = -i64::from(request.priority);
            self.pending
                .insert(counter, (neg_priority, request.clone()));
            self.heap.push(Reverse(QueueKey {
                neg_priority,
                counter,
            }));
        }

        tracing::info!(
            "Scheduler restored: {} requests, {} seen",
            data.requests.len(),
            data.seen.len()
        );
    }

    fn next_counter(&mut self) -> u64 {
        let counter = self.counter;
        self.counter += 1;
        counter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::SessionKwargs;
    use browser_core::types::HttpMethod;

    fn scheduler() -> Scheduler {
        Scheduler::new(FingerprintOptions::default())
    }

    fn urls(requests: &[Request]) -> Vec<&str> {
        requests.iter().map(|r| r.url.as_str()).collect()
    }

    #[test]
    fn duplicates_are_dropped_unless_dont_filter_is_set() {
        let mut s = scheduler();
        assert!(s.enqueue(Request::new("http://example.com/a")));
        assert!(!s.enqueue(Request::new("http://example.com/a")));
        assert!(s.enqueue(Request::new("http://example.com/a").with_dont_filter(true)));
        assert_eq!(s.len(), 2);
        assert_eq!(s.seen_len(), 1);
    }

    #[test]
    fn a_different_sid_or_method_is_not_a_duplicate() {
        let mut s = scheduler();
        assert!(s.enqueue(Request::new("http://example.com/a")));
        assert!(s.enqueue(Request::new("http://example.com/a").with_sid("other")));
        assert!(
            s.enqueue(
                Request::new("http://example.com/a").with_session_kwargs(SessionKwargs {
                    method: Some(HttpMethod::Post),
                    ..SessionKwargs::default()
                })
            )
        );
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn higher_priority_first_then_fifo() {
        let mut s = scheduler();
        s.enqueue(Request::new("http://example.com/first"));
        s.enqueue(Request::new("http://example.com/high").with_priority(10));
        s.enqueue(Request::new("http://example.com/second"));
        s.enqueue(Request::new("http://example.com/mid").with_priority(5));
        s.enqueue(Request::new("http://example.com/low").with_priority(-1));

        let mut order = Vec::new();
        while let Some((ticket, request)) = s.dequeue() {
            order.push(request.url.clone());
            s.complete(ticket);
        }
        assert_eq!(
            order,
            vec![
                "http://example.com/high",
                "http://example.com/mid",
                "http://example.com/first",
                "http://example.com/second",
                "http://example.com/low",
            ]
        );
    }

    #[test]
    fn len_counts_the_queue_but_snapshot_counts_the_in_flight_too() {
        let mut s = scheduler();
        s.enqueue(Request::new("http://example.com/a"));
        s.enqueue(Request::new("http://example.com/b"));
        assert_eq!(s.len(), 2);

        let (ticket, _) = s.dequeue().expect("queued");
        assert_eq!(s.len(), 1);
        assert_eq!(s.inflight_len(), 1);
        let (snapshot, _) = s.snapshot();
        assert_eq!(
            urls(&snapshot),
            ["http://example.com/a", "http://example.com/b"]
        );

        s.complete(ticket);
        let (snapshot, _) = s.snapshot();
        assert_eq!(urls(&snapshot), ["http://example.com/b"]);
    }

    #[test]
    fn snapshot_is_in_priority_order() {
        let mut s = scheduler();
        s.enqueue(Request::new("http://example.com/p1").with_priority(1));
        s.enqueue(Request::new("http://example.com/p10").with_priority(10));
        s.enqueue(Request::new("http://example.com/p5").with_priority(5));

        let (snapshot, seen) = s.snapshot();
        assert_eq!(
            urls(&snapshot),
            [
                "http://example.com/p10",
                "http://example.com/p5",
                "http://example.com/p1"
            ]
        );
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn snapshot_returns_copies() {
        let mut s = scheduler();
        s.enqueue(Request::new("http://example.com/a"));
        let (mut snapshot, mut seen) = s.snapshot();
        snapshot.clear();
        seen.clear();
        assert_eq!(s.pending_len(), 1);
        assert_eq!(s.seen_len(), 1);
    }

    #[test]
    fn complete_is_idempotent_and_ignores_unknown_tickets() {
        let mut s = scheduler();
        s.enqueue(Request::new("http://example.com/a"));
        s.enqueue(Request::new("http://example.com/b"));
        let (ticket, _) = s.dequeue().expect("queued");
        s.complete(ticket);
        s.complete(ticket);
        assert_eq!(s.pending_len(), 1);

        let (other, _) = s.dequeue().expect("queued");
        s.complete(other);
        s.complete(ticket);
        assert_eq!(s.pending_len(), 0);
    }

    #[test]
    fn the_same_request_enqueued_twice_needs_two_completions() {
        let mut s = scheduler();
        let request = Request::new("http://example.com/a").with_dont_filter(true);
        assert!(s.enqueue(request.clone()));
        assert!(s.enqueue(request));
        assert_eq!(s.pending_len(), 2);

        let (first, _) = s.dequeue().expect("queued");
        let (second, _) = s.dequeue().expect("queued");
        assert_ne!(first, second);

        s.complete(first);
        assert_eq!(s.pending_len(), 1);
        s.complete(second);
        assert_eq!(s.pending_len(), 0);
    }

    #[test]
    fn restore_replays_the_queue_and_the_seen_set() {
        let mut source = scheduler();
        source.enqueue(Request::new("http://example.com/a").with_priority(1));
        source.enqueue(Request::new("http://example.com/b").with_priority(9));
        source.enqueue(Request::new("http://example.com/gone"));
        let (ticket, _) = {
            // dequeue+complete the third one so it is *not* in the snapshot
            let mut found = None;
            while let Some((ticket, request)) = source.dequeue() {
                if request.url.ends_with("/gone") {
                    found = Some((ticket, request));
                    break;
                }
            }
            found.expect("gone request")
        };
        source.complete(ticket);

        let (requests, seen) = source.snapshot();
        let data = CheckpointData { requests, seen };

        let mut restored = scheduler();
        restored.restore(&data);
        assert_eq!(restored.len(), 2);

        // Priority order survives the round trip.
        let (_, first) = restored.dequeue().expect("queued");
        assert_eq!(first.url, "http://example.com/b");

        // A previously seen request is still filtered, including the completed one.
        assert!(!restored.enqueue(Request::new("http://example.com/gone")));
        assert!(!restored.enqueue(Request::new("http://example.com/a")));
    }

    #[test]
    fn restore_keeps_fingerprints_so_they_are_not_recomputed() {
        let mut source = scheduler();
        source.enqueue(Request::new("http://example.com/a"));
        let (requests, seen) = source.snapshot();
        assert!(requests[0].fingerprint().is_some());

        let mut restored = scheduler();
        restored.restore(&CheckpointData { requests, seen });
        let (_, request) = restored.dequeue().expect("queued");
        assert!(request.fingerprint().is_some());
    }

    #[test]
    fn is_empty_ignores_in_flight_requests() {
        let mut s = scheduler();
        s.enqueue(Request::new("http://example.com/a"));
        let (_ticket, _request) = s.dequeue().expect("queued");
        assert!(s.is_empty());
        assert_eq!(s.pending_len(), 1);
    }

    #[test]
    fn extreme_priorities_do_not_collide_into_one_queue_key() {
        // `-(i32::MIN)` does not fit in an `i32`. When the negation saturated, these two
        // priorities produced the same key and were ordered by insertion counter — the
        // wrong way round. Python's ints are unbounded and order them apart.
        let mut s = Scheduler::new(FingerprintOptions::DEFAULT);
        assert!(s.enqueue(Request::new("http://a/min").with_priority(i32::MIN)));
        assert!(s.enqueue(Request::new("http://a/min1").with_priority(i32::MIN + 1)));

        let (_, first) = s.dequeue().expect("queued");
        let (_, second) = s.dequeue().expect("queued");
        assert_eq!(first.url, "http://a/min1", "i32::MIN + 1 outranks i32::MIN");
        assert_eq!(second.url, "http://a/min");
    }
}
