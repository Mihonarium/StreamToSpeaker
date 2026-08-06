//! Thin wrapper around `QueryPerformanceCounter` / `QueryPerformanceFrequency`.
//!
//! The driver stamps every audio packet with a QPC value; we don't need it
//! for playback (Sonos has its own clock), but it's useful for jitter logs.

use std::time::Duration;

/// Returns the current QPC tick count, or `0` on non-Windows where the API
/// is unavailable (so calling code can still compile and run).
#[cfg(windows)]
pub fn query_performance_counter() -> u64 {
    let mut v: i64 = 0;
    // SAFETY: QPC writes a single i64; we pass a valid pointer.
    unsafe {
        windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut v);
    }
    v as u64
}

#[cfg(not(windows))]
pub fn query_performance_counter() -> u64 {
    0
}

/// Returns QPC frequency in ticks per second.
#[cfg(windows)]
pub fn query_performance_frequency() -> u64 {
    let mut v: i64 = 0;
    // SAFETY: same as above.
    unsafe {
        windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut v);
    }
    if v <= 0 {
        // Should not happen on a healthy Windows system but be safe.
        1
    } else {
        v as u64
    }
}

#[cfg(not(windows))]
pub fn query_performance_frequency() -> u64 {
    1_000_000_000
}

/// Convert a QPC delta (`ticks_later - ticks_earlier`) to a Duration.
pub fn qpc_delta_to_duration(ticks: u64) -> Duration {
    let freq = query_performance_frequency();
    let secs = ticks / freq;
    let rem = ticks % freq;
    // nanos = rem * 1e9 / freq, but careful about overflow.
    let nanos = (rem as u128 * 1_000_000_000u128 / freq as u128) as u64;
    Duration::new(secs, nanos as u32)
}
