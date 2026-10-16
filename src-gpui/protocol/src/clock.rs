//! Testable time for the connection layer. Every timer in the ported TS
//! semantics (`src/lib/acp.ts` connect deadline, `src/lib/hub.ts` fetch
//! deadline, the appStore reconnect loop's backoff/probe/watchdog timers in
//! `src/store/appStore.ts`) goes through [`Clock`], so the ported unit tests
//! drive simulated time deterministically instead of sleeping.
//!
//! `MockClock::advance` mirrors `vi.advanceTimersByTimeAsync`: sleeps whose
//! deadline passed fire in deadline order (registration order among equals)
//! and the runtime is yielded so woken tasks actually run before the caller
//! observes state.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// A boxed, sendable future — the object-safe async return type used by the
/// transport traits in this crate (no `async fn in trait` dyn-compatibility
/// games needed).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Injected time source: monotonic-enough milliseconds plus a sleep.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
    /// Resolves after `ms` have passed (wall-clock semantics like `Date.now`
    /// deltas; deadlines never observe the clock going backwards).
    fn sleep_ms(&self, ms: u64) -> BoxFuture<'static, ()>;
}

/// Real time via `std` + tokio timers. Production wiring.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioClock;

impl Clock for TokioClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn sleep_ms(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)))
    }
}

struct MockState {
    now_ms: u64,
    /// (deadline, waker) — the sender half of a registered sleep. A dropped
    /// receiver (a sleep future that was created but never polled, or a
    /// deadline race already lost) is simply ignored at fire time, exactly
    /// like a cleared timeout whose callback was already detached.
    waiters: Vec<(u64, tokio::sync::oneshot::Sender<()>)>,
}

impl Default for MockState {
    fn default() -> Self {
        Self { now_ms: 0, waiters: Vec::new() }
    }
}

/// Deterministic simulated clock. Registration happens at `sleep_ms` CALL
/// time (like `setTimeout`), not at first poll.
#[derive(Clone, Default)]
pub struct MockClock(Arc<Mutex<MockState>>);

impl MockClock {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(MockState {
            now_ms: 0,
            waiters: Vec::new(),
        })))
    }

    /// Advance simulated time by `ms`, firing every sleep whose deadline has
    /// passed, then yield repeatedly so tasks woken by those timers (and by
    /// the oneshot chains they trigger) run before the caller continues.
    pub async fn advance(&self, ms: u64) {
        {
            let mut st = self.0.lock().unwrap();
            st.now_ms += ms;
            let now = st.now_ms;
            // Stable sort: same-deadline timers fire in registration order.
            st.waiters.sort_by_key(|(deadline, _)| *deadline);
            let mut i = 0;
            while i < st.waiters.len() {
                if st.waiters[i].0 <= now {
                    let (_, tx) = st.waiters.remove(i);
                    let _ = tx.send(());
                } else {
                    i += 1;
                }
            }
        }
        // Let woken (and transitively woken) tasks run. The chain depth any
        // single advance can produce is bounded well below this.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }
}

impl Clock for MockClock {
    fn now_ms(&self) -> u64 {
        self.0.lock().unwrap().now_ms
    }

    fn sleep_ms(&self, ms: u64) -> BoxFuture<'static, ()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut st = self.0.lock().unwrap();
        let deadline = st.now_ms + ms;
        st.waiters.push((deadline, tx));
        Box::pin(async move {
            let _ = rx.await;
        })
    }
}
