//! Whether the connect running on this thread is still wanted.
//!
//! A connect runs on a thread of its own; when a newer choice supersedes
//! it, the connect stops at its next step (between requests and attempts,
//! and before it binds) instead of setting up a speaker nobody wants. The
//! check is per thread, so the protocol code that asks needs no new
//! argument: it asks [`cancelled`].

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Once};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

type Check = Arc<dyn Fn() -> bool + Send + Sync>;

thread_local! {
    static CURRENT: RefCell<Option<Check>> = const { RefCell::new(None) };
}

/// A connect job and the binding made on its behalf. A delay reconnect
/// can carry the same job onto the thread that applies the change.
pub(crate) struct Binding {
    pub cancelled: AtomicBool,
    pub number: AtomicU64,
    first_answer: Once,
    on_answer: Box<dyn Fn() + Send + Sync>,
}

thread_local! {
    static BINDING: RefCell<Option<Arc<Binding>>> = const { RefCell::new(None) };
    static ASKING: Cell<bool> = const { Cell::new(false) };
}

impl Binding {
    pub fn new(on_answer: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            number: AtomicU64::new(0),
            first_answer: Once::new(),
            on_answer: Box::new(on_answer),
        }
    }

    pub fn enter(self: &Arc<Self>) -> Scope {
        let job = self.clone();
        let scope = enter(Arc::new(move || job.cancelled.load(Ordering::SeqCst)));
        BINDING.with(|b| *b.borrow_mut() = Some(self.clone()));
        scope
    }
}

pub(crate) fn binding() -> Option<Arc<Binding>> {
    BINDING.with(|b| b.borrow().clone())
}

/// While this scope lives, replies are from the speaker being asked.
/// The same connect thread can also tear down a different speaker.
pub(crate) fn asking_speaker() -> Asking {
    Asking(ASKING.replace(true))
}

pub(crate) struct Asking(bool);

impl Drop for Asking {
    fn drop(&mut self) {
        ASKING.set(self.0);
    }
}

/// Report the first reply across the look and all attempts made by this job.
pub(crate) fn speaker_answered() {
    if ASKING.get() {
        BINDING.with_borrow(|job| {
            if let Some(job) = job {
                job.first_answer.call_once(|| (job.on_answer)());
            }
        });
    }
}

/// While alive, [`cancelled`] on this thread asks `check`.
pub struct Scope(());

/// Make `check` the answer to [`cancelled`] on this thread until the
/// returned scope is dropped.
pub fn enter(check: Check) -> Scope {
    CURRENT.with(|c| *c.borrow_mut() = Some(check));
    Scope(())
}

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = None);
        BINDING.with(|b| *b.borrow_mut() = None);
    }
}

/// Whether the connect on this thread has been superseded. False on a
/// thread that isn't running one.
pub fn cancelled() -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(|check| check()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_scope_answers_only_on_its_own_thread_and_only_while_alive() {
        let flag = Arc::new(AtomicBool::new(false));
        let f = flag.clone();
        let scope = enter(Arc::new(move || f.load(Ordering::SeqCst)));
        assert!(!cancelled());
        flag.store(true, Ordering::SeqCst);
        assert!(cancelled());
        assert!(!std::thread::spawn(cancelled).join().unwrap(), "another thread isn't affected");
        drop(scope);
        assert!(!cancelled());
    }
}
