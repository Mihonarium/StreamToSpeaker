//! The engine: the one owner of where the sound plays. It takes the
//! model's commands in order on a thread of its own, runs each connect on
//! a thread that ends with it, follows Windows through the platform seam,
//! and turns what it knows into the model's snapshot.

pub mod cancel;
pub mod core;
mod driver;
mod facts;

pub use driver::{Engine, Opener};

/// How many threads this process has now (Linux; 0 where it can't tell).
pub fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0)
}
