//! Bidirectional volume sync, with a debounce so changes don't bounce.
//!
//! Conversions between Windows millibels (range -10000..=0; UPnP-friendly
//! log mapping) and the Sonos 0..100 linear volume scale:
//!
//!  * Windows: log scale, 0 mB = 100%, -2000 mB = -20 dB ≈ 10% perceived.
//!  * Sonos: linear-ish UI scale but the speaker maps it internally.
//!
//! We use the common heuristic: `level = 100 * 10^(mb / 5000)`, clamped.
//! Inverse: `mb = 5000 * log10(level/100)`. This roughly matches what
//! Sonos and Windows agree on subjectively. Not strictly correct, but
//! consistent.

use log::debug;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Window inside which we ignore "echoes" of changes we just made.
const ECHO_IGNORE_WINDOW: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub struct VolumeSync {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Last time we pushed a volume to Sonos (so we can ignore the echo
    /// notify that follows).
    last_pushed_to_sonos: Option<(u32, Instant)>,
    /// Last time we pushed a volume to the driver.
    last_pushed_to_driver: Option<(i32, Instant)>,
    /// Most recent committed values, for comparison.
    last_sonos_volume: Option<u32>,
    last_driver_mb: Option<i32>,
}

impl VolumeSync {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Decision wrapper: should we forward a driver volume change to
    /// Sonos? Returns the Sonos-scale level if yes.
    pub fn driver_changed(&self, level_millibels: i32) -> Option<u32> {
        let sonos_level = millibels_to_sonos(level_millibels);
        let mut inner = self.inner.lock().unwrap();
        // If we *just* pushed this value to the driver (from a Sonos
        // change), drop this echo.
        if let Some((mb, t)) = inner.last_pushed_to_driver {
            if t.elapsed() < ECHO_IGNORE_WINDOW && (mb - level_millibels).abs() < 50 {
                debug!("ignoring driver echo: mb={} (we pushed {} {:?} ago)",
                    level_millibels, mb, t.elapsed());
                return None;
            }
        }
        // No-op if value didn't actually change.
        if Some(sonos_level) == inner.last_sonos_volume {
            return None;
        }
        inner.last_pushed_to_sonos = Some((sonos_level, Instant::now()));
        inner.last_sonos_volume = Some(sonos_level);
        inner.last_driver_mb = Some(level_millibels);
        Some(sonos_level)
    }

    /// Decision wrapper: should we forward a Sonos volume change to the
    /// driver? Returns the millibels value if yes.
    pub fn sonos_changed(&self, sonos_level: u32) -> Option<i32> {
        let mb = sonos_to_millibels(sonos_level);
        let mut inner = self.inner.lock().unwrap();
        if let Some((lvl, t)) = inner.last_pushed_to_sonos {
            if t.elapsed() < ECHO_IGNORE_WINDOW && lvl == sonos_level {
                debug!("ignoring sonos echo: level={} (we pushed {} {:?} ago)",
                    sonos_level, lvl, t.elapsed());
                return None;
            }
        }
        if Some(mb) == inner.last_driver_mb {
            return None;
        }
        inner.last_pushed_to_driver = Some((mb, Instant::now()));
        inner.last_driver_mb = Some(mb);
        inner.last_sonos_volume = Some(sonos_level);
        Some(mb)
    }
}

impl VolumeSync {
    /// Last-known Sonos-side volume, set by either of the *_changed
    /// paths above or by `prime_initial_volume`. The GUI reads this
    /// to render the volume slider (m24).
    pub fn current_level(&self) -> Option<u32> {
        self.inner.lock().unwrap().last_sonos_volume
    }

    /// Seed the cache from an initial `upnp::get_volume` call. Doesn't
    /// touch the `last_pushed_*` echo timestamps — the caller hasn't
    /// pushed anything, just observed.
    pub fn prime_initial_volume(&self, level: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.last_sonos_volume = Some(level);
        inner.last_driver_mb = Some(sonos_to_millibels(level));
    }
}

impl Default for VolumeSync {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert Windows millibels (-10000..=0) to a Sonos 0..100 scale.
pub fn millibels_to_sonos(mb: i32) -> u32 {
    if mb >= 0 {
        return 100;
    }
    if mb <= -10000 {
        return 0;
    }
    // mb is in millibels; convert to dB then to linear scale ratio.
    // Formula: ratio = 10^(mb / 5000); level = round(100 * ratio).
    let exp = (mb as f64) / 5000.0;
    let ratio = (10f64).powf(exp);
    let level = (100.0 * ratio).round().clamp(0.0, 100.0);
    level as u32
}

/// Inverse of `millibels_to_sonos`.
pub fn sonos_to_millibels(level: u32) -> i32 {
    let level = level.min(100);
    if level == 0 {
        return -10000;
    }
    if level >= 100 {
        return 0;
    }
    let ratio = (level as f64) / 100.0;
    let mb = (5000.0 * ratio.log10()).round() as i32;
    mb.clamp(-10000, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millibels_roundtrip() {
        for level in [0, 1, 10, 25, 50, 75, 100] {
            let mb = sonos_to_millibels(level);
            let back = millibels_to_sonos(mb);
            assert!((back as i32 - level as i32).abs() <= 1,
                "level {} -> {} -> {}", level, mb, back);
        }
        assert_eq!(millibels_to_sonos(0), 100);
        assert_eq!(millibels_to_sonos(-10000), 0);
    }

    #[test]
    fn echo_is_ignored() {
        let s = VolumeSync::new();
        // Sonos tells us volume=50.
        assert_eq!(s.sonos_changed(50), Some(sonos_to_millibels(50)));
        // Driver immediately echoes the new mb back.  Ignored.
        let mb = sonos_to_millibels(50);
        assert_eq!(s.driver_changed(mb), None);
    }
}
