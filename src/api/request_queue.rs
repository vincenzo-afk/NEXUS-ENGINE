//! A bounded-concurrency admission queue: caps how many requests of a
//! given kind (search, crawl, AI-summarize) run at once, queueing the
//! rest with a wait-time budget rather than either running unlimited
//! concurrent work (risking resource exhaustion under load) or rejecting
//! outright the instant a limit is hit (which `crate::api::rate_limit`'s
//! `concurrent_request_limit` already does at the connection level).
//! This is a softer, queue-then-serve layer specifically for expensive
//! internal operations (e.g. AI summarization calls, which
//! `crate::ai::client` shows are already rate-limited *externally*, but
//! nothing today limits how many run concurrently *internally* before
//! they even reach that external call).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

struct QueueState {
    in_flight: usize,
    waiting: usize,
}

/// A named, bounded-concurrency gate. Cheap to clone (an `Arc` inside)
/// so it can be shared across request-handling threads the way
/// `crate::api::mod`'s `Arc<Index>`/`Arc<RateLimiter>` already are.
#[derive(Clone)]
pub struct RequestQueue {
    name: &'static str,
    max_concurrent: usize,
    state: Arc<(Mutex<QueueState>, Condvar)>,
    admitted_total: Arc<AtomicU64>,
    timed_out_total: Arc<AtomicU64>,
}

/// RAII guard representing one admitted slot; dropping it releases the
/// slot and wakes the next waiter, so callers can't forget to release
/// (no matter which return path/panic unwinds through their handler).
pub struct QueuePermit {
    state: Arc<(Mutex<QueueState>, Condvar)>,
}

impl Drop for QueuePermit {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().expect("request queue mutex poisoned");
        state.in_flight = state.in_flight.saturating_sub(1);
        cvar.notify_one();
    }
}

impl RequestQueue {
    pub fn new(name: &'static str, max_concurrent: usize) -> Self {
        RequestQueue {
            name,
            max_concurrent: max_concurrent.max(1),
            state: Arc::new((
                Mutex::new(QueueState {
                    in_flight: 0,
                    waiting: 0,
                }),
                Condvar::new(),
            )),
            admitted_total: Arc::new(AtomicU64::new(0)),
            timed_out_total: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Blocks until a concurrency slot is free or `max_wait` elapses.
    /// Returns `Some(permit)` on admission, `None` on timeout (the
    /// caller should respond with a 429/503-style backpressure signal,
    /// not silently proceed unadmitted).
    pub fn acquire(&self, max_wait: Duration) -> Option<QueuePermit> {
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().expect("request queue mutex poisoned");
        state.waiting += 1;
        let deadline = Instant::now() + max_wait;

        while state.in_flight >= self.max_concurrent {
            let now = Instant::now();
            if now >= deadline {
                state.waiting -= 1;
                self.timed_out_total.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "request queue '{}' timed out waiting for a slot (max_concurrent={})",
                    self.name,
                    self.max_concurrent
                );
                return None;
            }
            let (guard, _timeout_result) = cvar
                .wait_timeout(state, deadline - now)
                .expect("request queue condvar wait poisoned");
            state = guard;
        }

        state.waiting -= 1;
        state.in_flight += 1;
        self.admitted_total.fetch_add(1, Ordering::Relaxed);
        Some(QueuePermit {
            state: Arc::clone(&self.state),
        })
    }

    /// Current queue depth (requests waiting for a slot), for the
    /// `/metrics` endpoint — a persistently nonzero/growing depth is the
    /// signal that `max_concurrent` for this queue is undersized for
    /// real load.
    pub fn queue_depth(&self) -> usize {
        self.state.0.lock().expect("request queue mutex poisoned").waiting
    }

    pub fn in_flight(&self) -> usize {
        self.state.0.lock().expect("request queue mutex poisoned").in_flight
    }

    pub fn admitted_total(&self) -> u64 {
        self.admitted_total.load(Ordering::Relaxed)
    }

    pub fn timed_out_total(&self) -> u64 {
        self.timed_out_total.load(Ordering::Relaxed)
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn admits_up_to_the_concurrency_limit() {
        let queue = RequestQueue::new("test", 2);
        let p1 = queue.acquire(Duration::from_millis(100));
        let p2 = queue.acquire(Duration::from_millis(100));
        assert!(p1.is_some());
        assert!(p2.is_some());
        assert_eq!(queue.in_flight(), 2);
    }

    #[test]
    fn third_request_waits_then_times_out_if_no_slot_frees() {
        let queue = RequestQueue::new("test", 1);
        let _held = queue.acquire(Duration::from_millis(50)).unwrap();
        let second = queue.acquire(Duration::from_millis(50));
        assert!(second.is_none(), "should time out with the only slot held");
        assert_eq!(queue.timed_out_total(), 1);
    }

    #[test]
    fn releasing_a_permit_frees_the_slot_for_the_next_waiter() {
        let queue = RequestQueue::new("test", 1);
        let permit = queue.acquire(Duration::from_millis(100)).unwrap();
        let queue_clone = queue.clone();
        let handle = thread::spawn(move || queue_clone.acquire(Duration::from_millis(500)).is_some());
        thread::sleep(Duration::from_millis(20));
        drop(permit); // release the slot
        assert!(handle.join().unwrap(), "waiter should be admitted once the slot frees");
    }
}
