//! When WPE's main loop has to run next.
//!
//! WPE's engine work is `GLib`'s: timers, socket completions from the network and
//! web processes, and the idle sources every DOM mutation is queued on all live
//! on one `GMainContext`. Nothing of that happens on its own — a `GMainContext`
//! runs when somebody iterates it — so the question this module answers is when
//! the host has to iterate it again, taken from the context itself rather than
//! guessed at.

use std::ffi::c_int;
use std::time::{Duration, Instant};

/// How long the pump waits when nothing but a descriptor can wake the engine.
///
/// `GLib`'s timeout says when the next *timer* is due, and nothing at all about
/// when one of [`WpeReadiness::descriptors`] becomes readable. A host that does
/// not watch those descriptors therefore has to look again on a bound of its
/// own, and this is that bound: the same 30 Hz ceiling the CEF backend puts on
/// Chromium's external message pump. It is a ceiling, never an interval — a
/// deadline `GLib` asked for is always shorter, and a host that folds the
/// descriptors into its own wakeup drives [`crate::WpeRuntime::pump`] from that
/// and never waits this long.
const MAXIMUM_PUMP_INTERVAL: Duration = Duration::from_millis(1000 / 30);

/// One descriptor the runtime's `GLib` main context wants watched.
///
/// A host whose event loop can watch descriptors adds these to it and iterates
/// the runtime when one signals, which is what lets an idle web view answer a
/// network completion the moment it lands instead of at the next bound wakeup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WpePollFd {
    fd: c_int,
    events: i16,
}

impl WpePollFd {
    pub(crate) const fn new(fd: c_int, events: i16) -> Self {
        Self { fd, events }
    }

    /// The descriptor to watch.
    #[must_use]
    pub const fn fd(self) -> c_int {
        self.fd
    }

    /// The `poll(2)` event mask `GLib` asked for on it.
    #[must_use]
    pub const fn events(self) -> i16 {
        self.events
    }
}

/// What the WPE runtime's `GLib` main context is currently waiting for.
///
/// One `g_main_context_prepare` / `g_main_context_query` pass, which asks every
/// source when it next wants to run and dispatches nothing.
#[derive(Debug, Clone)]
pub struct WpeReadiness {
    ready: bool,
    timeout: Option<Duration>,
    descriptors: Vec<WpePollFd>,
}

impl WpeReadiness {
    /// Builds a report from the raw ABI answer, where a negative timeout means
    /// no source has one.
    pub(crate) fn new(ready: bool, timeout_ms: i32, descriptors: Vec<WpePollFd>) -> Self {
        Self {
            ready,
            timeout: u64::try_from(timeout_ms).ok().map(Duration::from_millis),
            descriptors,
        }
    }

    /// Whether a source can be dispatched right now.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.ready
    }

    /// How long until the earliest timer source is due, when one exists.
    #[must_use]
    pub const fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// The descriptors the context wants watched.
    #[must_use]
    pub fn descriptors(&self) -> &[WpePollFd] {
        &self.descriptors
    }

    /// How long a host that does not watch [`Self::descriptors`] may wait.
    #[must_use]
    pub fn wait(&self) -> Duration {
        if self.ready {
            return Duration::ZERO;
        }
        self.timeout.map_or(MAXIMUM_PUMP_INTERVAL, |timeout| {
            timeout.min(MAXIMUM_PUMP_INTERVAL)
        })
    }

    /// The instant that wait is over.
    #[must_use]
    pub fn deadline(&self) -> PumpDeadline {
        PumpDeadline(Instant::now() + self.wait())
    }
}

/// The instant WPE's main loop next has to be iterated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpDeadline(Instant);

impl PumpDeadline {
    /// Returns the requested instant.
    #[must_use]
    pub const fn instant(self) -> Instant {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{MAXIMUM_PUMP_INTERVAL, WpePollFd, WpeReadiness};
    use std::time::Duration;

    /// A context with work in hand is not waited on at all, whatever timeout it
    /// also reported: `ready` is `GLib` saying "dispatch me now".
    #[test]
    fn a_ready_context_is_iterated_without_waiting() {
        assert_eq!(
            WpeReadiness::new(true, 5_000, Vec::new()).wait(),
            Duration::ZERO
        );
    }

    /// The deadline `GLib` asked for is the deadline that is used. This is the
    /// whole point of the readiness call: a background timer that wants to run
    /// in 20 ms gets 20 ms, not a fixed tick.
    #[test]
    fn a_timer_deadline_is_taken_from_the_context() {
        assert_eq!(
            WpeReadiness::new(false, 20, Vec::new()).wait(),
            Duration::from_millis(20)
        );
    }

    /// A timer further out than the ceiling still gets looked at on the
    /// ceiling, because a descriptor may signal long before that timer is due.
    #[test]
    fn a_distant_timer_is_capped_so_descriptors_are_still_looked_at() {
        assert_eq!(
            WpeReadiness::new(false, 60_000, Vec::new()).wait(),
            MAXIMUM_PUMP_INTERVAL
        );
    }

    /// `GLib` reports -1 when no source has a timeout at all — an idle page whose
    /// only pending work is a socket. That is exactly the case the ceiling
    /// exists for, and it must not become an infinite wait.
    #[test]
    fn a_context_with_no_timer_waits_the_ceiling_rather_than_forever() {
        let readiness = WpeReadiness::new(false, -1, Vec::new());

        assert_eq!(readiness.timeout(), None);
        assert_eq!(readiness.wait(), MAXIMUM_PUMP_INTERVAL);
    }

    /// A timer that is already due is a zero wait rather than a skipped one.
    #[test]
    fn an_expired_timer_is_iterated_immediately() {
        assert_eq!(
            WpeReadiness::new(false, 0, Vec::new()).wait(),
            Duration::ZERO
        );
    }

    /// The descriptors cross unchanged, so a host can put them straight into
    /// its own `poll(2)` set.
    #[test]
    fn descriptors_are_reported_as_the_context_gave_them() {
        let readiness = WpeReadiness::new(false, -1, vec![WpePollFd::new(7, 0x1)]);

        assert_eq!(readiness.descriptors().len(), 1);
        assert_eq!(readiness.descriptors()[0].fd(), 7);
        assert_eq!(readiness.descriptors()[0].events(), 0x1);
    }
}
