//! The chapter list's bounded auto-retry (#89), as a pure state machine so the
//! policy is host-tested rather than living untested inside the firmware loop.
//!
//! The case: the list fetch races DHCP right after `Hold::Story` is raised on
//! open, fails once with `connect failed`, and before #89 left the error on
//! glass until the user re-tapped LIST. Now a failure earns ONE retry after
//! [`ListRetry::DELAY_SECS`]; a second failure is shown. Success, or a fresh
//! open, restores the budget. Time is plain seconds so the caller can feed it
//! whatever clock it has.

/// What the caller should do with a failed fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFail {
    /// Keep the spinner up: a retry is scheduled.
    RetryScheduled,
    /// The retry was already spent: show the error.
    ShowError,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListRetry {
    failed_at: Option<u64>,
    spent: bool,
}

impl ListRetry {
    /// Seconds between the failure and the one retry.
    pub const DELAY_SECS: u64 = 5;

    pub const fn new() -> Self {
        Self { failed_at: None, spent: false }
    }

    /// A fetch failed at `now`.
    pub fn on_fail(&mut self, now: u64) -> OnFail {
        if self.spent {
            self.failed_at = None;
            OnFail::ShowError
        } else {
            self.spent = true;
            self.failed_at = Some(now);
            OnFail::RetryScheduled
        }
    }

    /// True exactly once, when the scheduled retry comes due.
    pub fn due(&mut self, now: u64) -> bool {
        match self.failed_at {
            Some(t) if now.saturating_sub(t) >= Self::DELAY_SECS => {
                self.failed_at = None;
                true
            }
            _ => false,
        }
    }

    /// A fetch succeeded, or the app was freshly opened: full budget again.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_failure_schedules_one_retry_and_the_second_shows_the_error() {
        let mut r = ListRetry::new();
        assert_eq!(r.on_fail(100), OnFail::RetryScheduled);
        assert!(!r.due(100 + ListRetry::DELAY_SECS - 1), "not before the delay");
        assert!(r.due(100 + ListRetry::DELAY_SECS), "due at the delay");
        assert!(!r.due(200), "due fires once, not every tick after");
        assert_eq!(r.on_fail(206), OnFail::ShowError, "bounded: one retry only");
        assert!(!r.due(1_000), "a shown error schedules nothing");
    }

    #[test]
    fn success_or_a_fresh_open_restores_the_budget() {
        let mut r = ListRetry::new();
        assert_eq!(r.on_fail(1), OnFail::RetryScheduled);
        r.reset();
        assert_eq!(r.on_fail(50), OnFail::RetryScheduled, "after reset, a retry again");
    }

    #[test]
    fn nothing_is_due_without_a_failure() {
        let mut r = ListRetry::new();
        assert!(!r.due(0));
        assert!(!r.due(u64::MAX));
    }
}
