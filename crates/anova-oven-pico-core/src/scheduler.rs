//! Bounded priority-ish event queue used by `api_client_task`.
//!
//! Events are keyed by `EventKind` and deduplicated by kind (at most one entry
//! per kind in the queue). `soonest_index` orders by `execution_time` first
//! and breaks ties by `priority`, so an `ApiStart`/`ApiStop` enqueued at the
//! same instant as a due poll pops first — but an *overdue* poll
//! (`execution_time < now`) still pops before a freshly-enqueued command at
//! `now`, since time sorts before priority. This is the behavior the bin's
//! drain loop relies on for §1.2's command-preemption.

use embassy_time::Instant;
use heapless::Vec as HeaplessVec;

/// Maximum number of pending events. Five distinct `EventKind`s exist and
/// `enqueue` dedups by kind, so 16 is comfortable headroom; overflow is
/// structurally impossible in normal operation.
pub const EVENT_QUEUE_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    PollStatus,
    PollCurrentCook,
    PollRecipes,
    ApiStart,
    ApiStop,
}

impl EventKind {
    /// Lower numbers sort first when tie-breaking equal `execution_time`s.
    /// Commands (`ApiStart`/`ApiStop`) outrank polls at the same instant.
    pub fn priority(self) -> u8 {
        match self {
            EventKind::ApiStart | EventKind::ApiStop => 0,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ScheduledEvent {
    pub kind: EventKind,
    pub execution_time: Instant,
    pub priority: u8,
}

#[derive(Clone, Copy, Debug)]
pub enum EnqueueMode {
    /// Existing entry of the same kind keeps the *earlier* of the two times.
    PreferEarlier,
    /// Existing entry of the same kind is overwritten with the new time.
    Replace,
}

/// Returned by [`EventQueue::enqueue`] when the queue is at capacity and the
/// event's kind isn't already pending. Logging the overflow is the caller's
/// concern (the lib stays effect-free).
#[derive(Debug, PartialEq, Eq)]
pub struct QueueOverflow;

pub struct EventQueue {
    events: HeaplessVec<ScheduledEvent, EVENT_QUEUE_CAPACITY>,
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl EventQueue {
    pub fn new() -> Self {
        Self {
            events: HeaplessVec::new(),
        }
    }

    /// Insert (or update) an event for `kind`. If an entry of the same kind
    /// is already pending, its `execution_time` is updated per `mode` and no
    /// new slot is consumed. Returns `Err(QueueOverflow)` only if the kind
    /// isn't pending *and* the queue is full.
    pub fn enqueue(
        &mut self,
        kind: EventKind,
        execution_time: Instant,
        mode: EnqueueMode,
    ) -> Result<(), QueueOverflow> {
        if let Some(existing) = self.events.iter_mut().find(|event| event.kind == kind) {
            existing.execution_time = match mode {
                EnqueueMode::PreferEarlier => existing.execution_time.min(execution_time),
                EnqueueMode::Replace => execution_time,
            };
            return Ok(());
        }

        self.events
            .push(ScheduledEvent {
                kind,
                execution_time,
                priority: kind.priority(),
            })
            .map_err(|_| QueueOverflow)
    }

    /// Index of the soonest-due event, ordered by `(execution_time, priority)`.
    /// Returns `None` if the queue is empty. On full ties, `min_by` returns
    /// the *first* element in vector order — which is insertion order until
    /// a `pop_due` calls `swap_remove`, after which vector order is scrambled.
    pub fn soonest_index(&self) -> Option<usize> {
        self.events
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.execution_time
                    .cmp(&b.execution_time)
                    .then(a.priority.cmp(&b.priority))
            })
            .map(|(idx, _)| idx)
    }

    pub fn next_due_at(&self) -> Option<Instant> {
        self.soonest_index()
            .map(|idx| self.events[idx].execution_time)
    }

    pub fn has_pending(&self, kind: EventKind) -> bool {
        self.events.iter().any(|event| event.kind == kind)
    }

    /// Pop the soonest event if its `execution_time <= now`. Removed via
    /// `swap_remove`, which is O(1) but does not preserve vector order.
    pub fn pop_due(&mut self, now: Instant) -> Option<ScheduledEvent> {
        let idx = self.soonest_index()?;
        if self.events[idx].execution_time > now {
            return None;
        }

        Some(self.events.swap_remove(idx))
    }

    /// Number of pending events. Useful for tests; the bin doesn't use it.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embassy_time::Duration;

    fn t(ticks: u64) -> Instant {
        Instant::from_ticks(ticks)
    }

    #[test]
    fn enqueue_adds_event() {
        let mut q = EventQueue::new();
        assert_eq!(q.enqueue(EventKind::PollStatus, t(100), EnqueueMode::PreferEarlier), Ok(()));
        assert_eq!(q.len(), 1);
        assert!(q.has_pending(EventKind::PollStatus));
    }

    #[test]
    fn enqueue_dedups_by_kind() {
        let mut q = EventQueue::new();
        q.enqueue(EventKind::PollStatus, t(100), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::PollStatus, t(200), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::PollStatus, t(50), EnqueueMode::PreferEarlier).unwrap();
        assert_eq!(q.len(), 1, "same kind enqueued thrice should occupy one slot");
        assert_eq!(q.next_due_at(), Some(t(50)), "PreferEarlier keeps the minimum");
    }

    #[test]
    fn prefer_earlier_picks_min_time() {
        let mut q = EventQueue::new();
        q.enqueue(EventKind::PollStatus, t(100), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::PollStatus, t(50), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::PollStatus, t(200), EnqueueMode::PreferEarlier).unwrap();
        assert_eq!(q.next_due_at(), Some(t(50)));
    }

    #[test]
    fn replace_overwrites_existing_time() {
        let mut q = EventQueue::new();
        q.enqueue(EventKind::PollStatus, t(100), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::PollStatus, t(50), EnqueueMode::Replace).unwrap();
        assert_eq!(q.next_due_at(), Some(t(50)));
        q.enqueue(EventKind::PollStatus, t(200), EnqueueMode::Replace).unwrap();
        assert_eq!(q.next_due_at(), Some(t(200)),
            "Replace overwrites even when the new time is later");
    }

    #[test]
    fn soonest_index_orders_by_time_first() {
        let mut q = EventQueue::new();
        q.enqueue(EventKind::PollStatus, t(500), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::PollCurrentCook, t(100), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::PollRecipes, t(300), EnqueueMode::PreferEarlier).unwrap();
        let idx = q.soonest_index().unwrap();
        assert_eq!(q.next_due_at(), Some(t(100)));
        // PollCurrentCook (the one with time=100) should be at that index.
        // We don't assert the index value itself (insertion order), only that
        // the event it points to is the soonest one.
        let popped = q.pop_due(t(100)).unwrap();
        assert_eq!(popped.kind, EventKind::PollCurrentCook);
        let _ = idx;
    }

    #[test]
    fn tie_broken_by_priority_command_wins_over_poll_at_same_instant() {
        let mut q = EventQueue::new();
        // Enqueue the poll first to prove this isn't insertion-order winning.
        q.enqueue(EventKind::PollStatus, t(100), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::ApiStop, t(100), EnqueueMode::PreferEarlier).unwrap();
        let popped = q.pop_due(t(100)).unwrap();
        assert_eq!(popped.kind, EventKind::ApiStop,
            "at equal execution_time, priority-0 command beats priority-1 poll");
    }

    #[test]
    fn overdue_poll_beats_just_enqueued_command() {
        // This is the residual §1.2 latency: a command does NOT preempt an
        // already-overdue poll, because execution_time sorts before priority.
        let mut q = EventQueue::new();
        q.enqueue(EventKind::PollStatus, t(50), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::ApiStop, t(100), EnqueueMode::PreferEarlier).unwrap();
        let popped = q.pop_due(t(100)).unwrap();
        assert_eq!(popped.kind, EventKind::PollStatus,
            "an overdue poll (t=50) outranks a just-enqueued command (t=100)");
    }

    #[test]
    fn pop_due_returns_none_when_nothing_due() {
        let mut q = EventQueue::new();
        q.enqueue(EventKind::PollStatus, t(1000), EnqueueMode::PreferEarlier).unwrap();
        assert!(q.pop_due(t(500)).is_none(), "future events are not yet due");
        assert_eq!(q.len(), 1, "non-due event stays in the queue");
    }

    #[test]
    fn pop_due_returns_none_when_empty() {
        let mut q = EventQueue::new();
        assert!(q.pop_due(t(1000)).is_none());
    }

    #[test]
    fn pop_due_removes_the_event() {
        let mut q = EventQueue::new();
        q.enqueue(EventKind::PollStatus, t(100), EnqueueMode::PreferEarlier).unwrap();
        q.enqueue(EventKind::PollCurrentCook, t(200), EnqueueMode::PreferEarlier).unwrap();
        q.pop_due(t(150)).unwrap();
        assert_eq!(q.len(), 1);
        assert!(!q.has_pending(EventKind::PollStatus));
        assert!(q.has_pending(EventKind::PollCurrentCook));
    }

    #[test]
    fn has_pending_reflects_state() {
        let mut q = EventQueue::new();
        assert!(!q.has_pending(EventKind::ApiStart));
        q.enqueue(EventKind::ApiStart, t(100), EnqueueMode::PreferEarlier).unwrap();
        assert!(q.has_pending(EventKind::ApiStart));
        q.pop_due(t(100));
        assert!(!q.has_pending(EventKind::ApiStart));
    }

    #[test]
    fn enqueue_full_returns_overflow_only_for_new_kind() {
        let mut q = EventQueue::new();
        // Fill with N distinct synthetic kinds via the five real kinds, then
        // try to push a new one. Since we only have five EventKind variants,
        // this scenario is structurally impossible in normal operation — but
        // we still verify the Result type by exhausting via a tighter cap
        // would require a generic. Instead, prove the dedup path doesn't
        // overflow even when "trying" repeatedly:
        for _ in 0..(EVENT_QUEUE_CAPACITY * 2) {
            assert!(q
                .enqueue(EventKind::PollStatus, t(100), EnqueueMode::PreferEarlier)
                .is_ok(), "repeated enqueues of same kind never overflow");
        }
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn duration_arithmetic_works_in_tests() {
        // Smoke test that Instant + Duration is usable without a TimeDriver
        // running (we use `from_ticks`, not `now`).
        let mut q = EventQueue::new();
        let now = t(1000);
        q.enqueue(EventKind::PollStatus, now + Duration::from_millis(250), EnqueueMode::PreferEarlier)
            .unwrap();
        assert!(q.pop_due(now).is_none());
        assert!(q.pop_due(now + Duration::from_millis(250)).is_some());
    }
}
