//! Ephemeral toast semantics — the port of the store-side contract pinned by
//! the TS suite `tests/notify-toast.test.ts`: a toast appears immediately,
//! auto-clears after its window, a newer toast replaces the old one AND
//! resets the clear timer (no stale clear), and dismissal clears
//! immediately.

/// The auto-clear window (`TOAST_MS` in appStore.ts).
pub const TOAST_MS: u64 = 4_000;

/// One showing toast: an id (monotonic, also the stale-clear guard) and its
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub id: u64,
    pub text: String,
}

/// notify()/dismissToast() with the timer factored into a deadline the
/// caller polls ([`ToastState::expire`]) — `now_ms` stands in for
/// `Date.now()`.
#[derive(Debug, Default)]
pub struct ToastState {
    toast: Option<Toast>,
    next_id: u64,
    deadline_ms: u64,
}

impl ToastState {
    pub fn new() -> Self {
        Self::default()
    }

    /// notify(text): shows immediately, replacing any current toast; the new
    /// toast's own deadline replaces the old one's.
    pub fn notify(&mut self, now_ms: u64, text: &str) -> Toast {
        self.next_id += 1;
        let toast = Toast {
            id: self.next_id,
            text: text.to_string(),
        };
        self.deadline_ms = now_ms.saturating_add(TOAST_MS);
        self.toast = Some(toast.clone());
        toast
    }

    pub fn current(&self) -> Option<&Toast> {
        self.toast.as_ref()
    }

    /// dismissToast(): clears immediately and cancels the timer.
    pub fn dismiss(&mut self) -> Option<Toast> {
        self.toast.take()
    }

    /// The clear-timer tick: clears the toast once its window has elapsed.
    /// Returns true when the toast expired NOW (the caller re-renders).
    pub fn expire(&mut self, now_ms: u64) -> bool {
        if self.toast.is_some() && now_ms >= self.deadline_ms {
            self.toast = None;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ports of tests/notify-toast.test.ts.

    #[test]
    fn notify_shows_a_toast_immediately_and_auto_clears_it() {
        let mut state = ToastState::new();
        assert_eq!(state.current(), None);
        state.notify(0, "downloading x");
        assert_eq!(state.current().unwrap().text, "downloading x");
        assert!(!state.expire(TOAST_MS - 1));
        assert_eq!(state.current().unwrap().text, "downloading x");
        assert!(state.expire(TOAST_MS));
        assert_eq!(state.current(), None);
    }

    #[test]
    fn a_newer_toast_replaces_the_older_one_and_survives_its_timer() {
        let mut state = ToastState::new();
        state.notify(0, "first");
        state.notify(3_000, "second");
        assert_eq!(state.current().unwrap().text, "second");
        // The first notify's deadline (0 + 4000) must NOT clear the newer
        // toast; only the newer one's own window (3000 + 4000) clears it.
        assert!(!state.expire(4_000));
        assert_eq!(state.current().unwrap().text, "second");
        assert!(!state.expire(6_999));
        assert_eq!(state.current().unwrap().text, "second");
        assert!(state.expire(7_000));
        assert_eq!(state.current(), None);
    }

    #[test]
    fn dismiss_clears_immediately_and_no_later_tick_resurrects_it() {
        let mut state = ToastState::new();
        state.notify(0, "hello");
        assert!(state.dismiss().is_some());
        assert_eq!(state.current(), None);
        // No pending timer fires on a later tick.
        assert!(!state.expire(10_000));
        assert_eq!(state.current(), None);
    }

    #[test]
    fn successive_toasts_carry_increasing_ids() {
        let mut state = ToastState::new();
        let a = state.notify(0, "a");
        let b = state.notify(1, "b");
        assert!(b.id > a.id);
    }
}
