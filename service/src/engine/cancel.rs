//! Whether the connect running on this thread is still wanted.
//!
//! A connect runs on a thread of its own; when a newer choice supersedes
//! it, the connect stops at its next step (between requests and attempts,
//! and before it binds) instead of setting up a speaker nobody wants. The
//! check is per thread, so the protocol code that asks needs no new
//! argument: it asks [`cancelled`].

use std::cell::RefCell;
use std::sync::Arc;

type Check = Arc<dyn Fn() -> bool + Send + Sync>;

thread_local! {
    static CURRENT: RefCell<Option<Check>> = const { RefCell::new(None) };
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
