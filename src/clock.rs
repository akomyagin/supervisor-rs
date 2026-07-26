//! Time abstraction so restart backoff logic can be tested without real sleeps.

use std::cell::Cell;
use std::time::{Duration, Instant};

pub trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, dur: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, dur: Duration) {
        std::thread::sleep(dur);
    }
}

/// Deterministic clock for tests: `sleep` advances the stored time instead of
/// blocking. Not `#[cfg(test)]`-gated because integration tests in `tests/`
/// cannot see test-only items of the crate.
pub struct FakeClock {
    now: Cell<Instant>,
}

impl FakeClock {
    pub fn new(start: Instant) -> Self {
        Self {
            now: Cell::new(start),
        }
    }

    pub fn advance(&self, dur: Duration) {
        self.now.set(self.now.get() + dur);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.now.get()
    }

    fn sleep(&self, dur: Duration) {
        self.advance(dur);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_advances_without_blocking() {
        let start = Instant::now();
        let clock = FakeClock::new(start);
        assert_eq!(clock.now(), start);

        clock.advance(Duration::from_secs(5));
        assert_eq!(clock.now(), start + Duration::from_secs(5));

        clock.sleep(Duration::from_secs(3));
        assert_eq!(clock.now(), start + Duration::from_secs(8));
    }
}
