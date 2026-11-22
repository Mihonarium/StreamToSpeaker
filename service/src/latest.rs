//! One worker that applies the latest value asked for: values overtaken
//! before their turn are skipped, a value already applied isn't applied
//! again, and a value counts as applied only once applying it worked. Used
//! where several places ask for something slow and only the last request
//! matters (the Windows device's name).

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct Latest<T> {
    shared: Arc<(Mutex<State<T>>, Condvar)>,
}

struct State<T> {
    /// Asked for and not taken by the worker yet.
    wanted: Option<T>,
    /// The last value applying worked for.
    applied: Option<T>,
    /// Requests made so far, and how many of them the worker has dealt
    /// with (applied, skipped or failed).
    asked: u64,
    done: u64,
}

impl<T: Clone + PartialEq + Send + 'static> Latest<T> {
    /// A worker thread named `name` that applies values with `apply`, which
    /// says whether it worked.
    pub fn new(name: &str, apply: impl Fn(&T) -> bool + Send + 'static) -> Self {
        let shared = Arc::new((
            Mutex::new(State { wanted: None, applied: None, asked: 0, done: 0 }),
            Condvar::new(),
        ));
        let worker = shared.clone();
        let spawned = std::thread::Builder::new().name(name.to_string()).spawn(move || {
            let (lock, wake) = &*worker;
            loop {
                let (value, upto, already) = {
                    let mut st = lock.lock().unwrap();
                    while st.wanted.is_none() {
                        st = wake.wait(st).unwrap();
                    }
                    let value = st.wanted.take().unwrap();
                    let already = st.applied.as_ref() == Some(&value);
                    (value, st.asked, already)
                };
                let worked = already || apply(&value);
                let mut st = lock.lock().unwrap();
                if worked {
                    st.applied = Some(value);
                }
                st.done = upto;
                wake.notify_all();
            }
        });
        if let Err(e) = spawned {
            log::warn!("couldn't start the {} worker: {}", name, e);
        }
        Self { shared }
    }

    /// Ask for `value`; returns at once. Its ticket for [`Latest::wait`].
    pub fn want(&self, value: T) -> u64 {
        let (lock, wake) = &*self.shared;
        let mut st = lock.lock().unwrap();
        st.wanted = Some(value);
        st.asked += 1;
        wake.notify_all();
        st.asked
    }

    /// Wait up to `max` until the request with `ticket` (or a later one)
    /// has been dealt with. Whether it was.
    pub fn wait(&self, ticket: u64, max: Duration) -> bool {
        let (lock, wake) = &*self.shared;
        let end = Instant::now() + max;
        let mut st = lock.lock().unwrap();
        while st.done < ticket {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = wake.wait_timeout(st, left).unwrap().0;
        }
        true
    }

    /// The last value applying worked for.
    #[cfg(test)]
    fn applied(&self) -> Option<T> {
        self.shared.0.lock().unwrap().applied.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{unbounded, Receiver, Sender};

    const PATIENCE: Duration = Duration::from_secs(5);

    /// A worker whose applying waits for the test's go-ahead, reporting
    /// each value it starts on; the go-ahead says whether it works.
    fn worker() -> (Latest<&'static str>, Receiver<&'static str>, Sender<bool>) {
        let (started_tx, started) = unbounded();
        let (go_tx, go) = unbounded::<bool>();
        let latest = Latest::new("test-latest", move |v: &&'static str| {
            let _ = started_tx.send(*v);
            go.recv().unwrap_or(false)
        });
        (latest, started, go_tx)
    }

    #[test]
    fn values_overtaken_while_one_is_applied_are_skipped() {
        let (latest, started, go) = worker();
        latest.want("kitchen");
        assert_eq!(started.recv_timeout(PATIENCE), Ok("kitchen"));
        // While that runs, two more requests: only the last one matters.
        latest.want("den");
        let last = latest.want("plain");
        go.send(true).unwrap();
        assert_eq!(started.recv_timeout(PATIENCE), Ok("plain"));
        go.send(true).unwrap();
        assert!(latest.wait(last, PATIENCE));
        assert_eq!(latest.applied(), Some("plain"));
        assert!(started.try_recv().is_err(), "applied a value that was overtaken");
    }

    #[test]
    fn a_value_counts_as_applied_only_once_it_worked() {
        let (latest, started, go) = worker();
        let first = latest.want("kitchen");
        assert_eq!(started.recv_timeout(PATIENCE), Ok("kitchen"));
        go.send(false).unwrap();
        assert!(latest.wait(first, PATIENCE));
        assert_eq!(latest.applied(), None);
        // Asked again: tried again.
        let again = latest.want("kitchen");
        assert_eq!(started.recv_timeout(PATIENCE), Ok("kitchen"));
        go.send(true).unwrap();
        assert!(latest.wait(again, PATIENCE));
        assert_eq!(latest.applied(), Some("kitchen"));
        // Asked once more: already applied, nothing to do.
        let third = latest.want("kitchen");
        assert!(latest.wait(third, PATIENCE));
        assert!(started.try_recv().is_err());
    }

    #[test]
    fn waiting_gives_up_after_its_time() {
        let (latest, started, go) = worker();
        let ticket = latest.want("kitchen");
        assert_eq!(started.recv_timeout(PATIENCE), Ok("kitchen"));
        let at = Instant::now();
        assert!(!latest.wait(ticket, Duration::from_millis(30)));
        assert!(at.elapsed() >= Duration::from_millis(30));
        go.send(true).unwrap();
        assert!(latest.wait(ticket, PATIENCE));
    }
}
