//! When a delay change reconnects an AirPlay speaker, a HomePod or a
//! Sendspin speaker.
//!
//! On these speakers a change applies by connecting again, which costs a
//! few seconds of silence. So the reconnect waits until the presses stop:
//! it begins [`AFTER_LAST_PRESS`] after the last one. Only one runs at a
//! time; presses made during one are gathered into one more, which begins
//! [`BETWEEN_RECONNECTS`] after the previous one began at the soonest. The
//! same spacing holds before trying the previous value again after a
//! reconnect the speaker didn't accept.

use std::time::{Duration, Instant};

/// How long after the last press the reconnect begins.
pub const AFTER_LAST_PRESS: Duration = Duration::from_millis(1500);
/// The least time between the beginnings of two reconnects.
pub const BETWEEN_RECONNECTS: Duration = Duration::from_secs(5);

/// The reconnects one speaker's delay changes ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// [`AFTER_LAST_PRESS`] and [`BETWEEN_RECONNECTS`] (shorter in tests).
    after_press: Duration,
    between: Duration,
    /// The last press still waiting for its reconnect.
    pressed_at: Option<Instant>,
    /// When the reconnect that runs began.
    running_since: Option<Instant>,
    /// When the last reconnect began.
    last_began: Option<Instant>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self::with(AFTER_LAST_PRESS, BETWEEN_RECONNECTS)
    }
}

impl Schedule {
    /// A schedule with these waits instead of the usual ones.
    pub fn with(after_press: Duration, between: Duration) -> Self {
        Self { after_press, between, pressed_at: None, running_since: None, last_began: None }
    }

    /// A press at `now` asks for a reconnect.
    pub fn press(&mut self, now: Instant) {
        self.pressed_at = Some(now);
    }

    /// Nothing waits any more (the speaker stopped playing, or another one
    /// plays now).
    pub fn cancel(&mut self) {
        self.pressed_at = None;
    }

    /// A reconnect waits for its time.
    pub fn waiting(&self) -> bool {
        self.pressed_at.is_some()
    }

    /// A reconnect runs.
    pub fn running(&self) -> bool {
        self.running_since.is_some()
    }

    /// When the reconnect that waits may begin; `None` when none waits, or
    /// while one runs.
    pub fn due_at(&self) -> Option<Instant> {
        if self.running_since.is_some() {
            return None;
        }
        let at = self.pressed_at? + self.after_press;
        Some(match self.last_began {
            Some(began) => at.max(began + self.between),
            None => at,
        })
    }

    /// Begin the reconnect that is due at `now`; false if none is.
    pub fn begin(&mut self, now: Instant) -> bool {
        if !self.due_at().is_some_and(|at| now >= at) {
            return false;
        }
        self.pressed_at = None;
        self.running_since = Some(now);
        self.last_began = Some(now);
        true
    }

    /// A second attempt within the same reconnect began at `now` (the
    /// previous value, after the new one wasn't accepted).
    pub fn began_again(&mut self, now: Instant) {
        self.last_began = Some(now);
    }

    /// When a second attempt may begin: the spacing from the first.
    pub fn again_at(&self) -> Option<Instant> {
        self.last_began.map(|b| b + self.between)
    }

    /// The reconnect that ran is over.
    pub fn finished(&mut self) {
        self.running_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_reconnect_begins_after_the_last_press() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        assert_eq!(s.due_at(), None);
        s.press(t0);
        assert_eq!(s.due_at(), Some(t0 + ms(1500)));
        // Another press moves it.
        s.press(t0 + ms(1000));
        assert_eq!(s.due_at(), Some(t0 + ms(2500)));
        assert!(!s.begin(t0 + ms(2499)));
        assert!(s.begin(t0 + ms(2500)));
        assert!(s.running() && !s.waiting());
    }

    #[test]
    fn one_reconnect_at_a_time_and_presses_meanwhile_make_one_more() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        s.press(t0);
        assert!(s.begin(t0 + ms(1500)));
        // Three presses during it: gathered, and nothing begins while it runs.
        for t in [2000, 2500, 3000] {
            s.press(t0 + ms(t));
            assert_eq!(s.due_at(), None);
            assert!(!s.begin(t0 + ms(t + 2000)));
        }
        s.finished();
        // 5 s after the previous began, though the last press was earlier.
        assert_eq!(s.due_at(), Some(t0 + ms(6500)));
        assert!(s.begin(t0 + ms(6500)));
        s.finished();
        // A press long after: 1.5 s after it.
        s.press(t0 + ms(20_000));
        assert_eq!(s.due_at(), Some(t0 + ms(21_500)));
    }

    #[test]
    fn a_second_attempt_keeps_the_spacing() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        s.press(t0);
        assert!(s.begin(t0 + ms(1500)));
        assert_eq!(s.again_at(), Some(t0 + ms(6500)));
        s.began_again(t0 + ms(6500));
        s.finished();
        s.press(t0 + ms(7000));
        assert_eq!(s.due_at(), Some(t0 + ms(11_500)));
    }

    #[test]
    fn a_cancelled_wait_begins_nothing() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        s.press(t0);
        s.cancel();
        assert_eq!(s.due_at(), None);
        assert!(!s.begin(t0 + ms(10_000)));
    }
}
