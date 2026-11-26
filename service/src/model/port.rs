//! The port between the engine and the surfaces: a surface reads the
//! current snapshot, learns that it changed, and sends commands.
//!
//! Surfaces run on UI threads, so sending never blocks and never does the
//! work on the caller's thread: an implementation queues the command and
//! its own thread handles it. A watcher is called on the engine's thread
//! after each new snapshot and must return at once (request a repaint, post
//! a message); it reads the snapshot itself later, on its own thread.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::command::{Command, Surface, Ticket};
use super::Snapshot;

/// Called after every new snapshot.
pub type Watcher = Arc<dyn Fn() + Send + Sync>;

/// What every surface talks to.
pub trait Port: Send + Sync {
    /// The current snapshot (an `Arc` clone; cheap).
    fn snapshot(&self) -> Arc<Snapshot>;

    /// Calls `watcher` after every new snapshot until the returned guard is
    /// dropped.
    fn watch(&self, watcher: Watcher) -> Watch;

    /// Queues `command` from `from`. Returns at once; the ticket gives the
    /// outcome once the engine has handled it, for surfaces that report it
    /// (the command line, the shortcut's confirmation).
    fn send(&self, from: Surface, command: Command) -> Ticket;

    /// The level meter: this PC's output level now, 0.0–1.0. Read as often
    /// as the meter repaints; changes too often to be in the snapshot.
    fn level(&self) -> f32;
}

/// Unregisters its watcher when dropped.
pub struct Watch {
    id: u64,
    watchers: Weak<Mutex<Vec<(u64, Watcher)>>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(w) = self.watchers.upgrade() {
            if let Ok(mut list) = w.lock() {
                list.retain(|(id, _)| *id != self.id);
            }
        }
    }
}

/// The snapshot, the watchers and the meter: the part of a [`Port`] every
/// implementation shares. The engine publishes; surfaces read.
pub struct PortCore {
    current: Mutex<Arc<Snapshot>>,
    version: AtomicU64,
    watchers: Arc<Mutex<Vec<(u64, Watcher)>>>,
    next_watch: AtomicU64,
    level_bits: AtomicU32,
}

impl PortCore {
    pub fn new(first: Snapshot) -> Self {
        PortCore {
            version: AtomicU64::new(first.version),
            current: Mutex::new(Arc::new(first)),
            watchers: Arc::new(Mutex::new(Vec::new())),
            next_watch: AtomicU64::new(1),
            level_bits: AtomicU32::new(0f32.to_bits()),
        }
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The next snapshot's version.
    pub fn next_version(&self) -> u64 {
        self.version.load(Ordering::Acquire) + 1
    }

    /// Makes `snapshot` current and tells the watchers. Watchers are called
    /// outside the lock, so one that reads the snapshot doesn't deadlock.
    pub fn publish(&self, snapshot: Snapshot) -> Arc<Snapshot> {
        let snapshot = Arc::new(snapshot);
        self.version.store(snapshot.version, Ordering::Release);
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = snapshot.clone();
        let watchers: Vec<Watcher> = self
            .watchers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, w)| w.clone())
            .collect();
        for w in watchers {
            w();
        }
        snapshot
    }

    pub fn watch(&self, watcher: Watcher) -> Watch {
        let id = self.next_watch.fetch_add(1, Ordering::Relaxed);
        self.watchers.lock().unwrap_or_else(|e| e.into_inner()).push((id, watcher));
        Watch { id, watchers: Arc::downgrade(&self.watchers) }
    }

    pub fn set_level(&self, level: f32) {
        self.level_bits.store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }
}
