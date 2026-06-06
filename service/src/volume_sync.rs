//! Bidirectional volume sync between the Windows endpoint slider and the
//! speaker, with echo suppression so a change made on one side doesn't
//! bounce back from the other.
//!
//! Both sides now speak the **same** linear `0..=100` scale:
//!
//!  * Windows: we read/write the `IAudioEndpointVolume` *scalar* (the
//!    actual slider position) via [`crate::endpoint_volume`], scaled to
//!    `0..=100`. The slider percentage maps 1:1 to the speaker.
//!  * Speaker (Sonos/UPnP): the `RenderingControl` `Volume` is already a
//!    `0..=100` value matching the speaker's own UI slider.
//!
//! So there is no scale conversion here — `VolumeSync` is purely a
//! debounce / echo-suppression / last-known-value cache.
//!
//! The millibel `<->` percent helpers below are a *fallback* for the
//! cold path where Core Audio can't reach our endpoint and we have to
//! work straight off the driver's dB IOCTL. They are an approximation of
//! Windows' proprietary, undocumented volume taper, so they are
//! deliberately only used when the exact scalar is unavailable.

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
    /// Last value (0..=100) we pushed to the speaker (so we can drop the
    /// GENA echo that follows).
    last_pushed_to_speaker: Option<(u32, Instant)>,
    /// Last value (0..=100) we pushed to the Windows slider (so we can
    /// drop the driver-event echo that follows).
    last_pushed_to_windows: Option<(u32, Instant)>,
    /// Most recent committed level, for dedup and for the GUI slider.
    last_level: Option<u32>,
}

impl VolumeSync {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The Windows slider moved (reported via the driver event, then read
    /// back as a scalar). `level` is `0..=100`. Returns the level to push
    /// to the speaker, or `None` if it's an echo / no change.
    pub fn driver_changed(&self, level: u32) -> Option<u32> {
        let level = level.min(100);
        let mut inner = self.inner.lock().unwrap();
        // If we *just* set this on the Windows slider (from a speaker
        // change), this is the echo — drop it.
        if let Some((lvl, t)) = inner.last_pushed_to_windows {
            if t.elapsed() < ECHO_IGNORE_WINDOW && lvl == level {
                debug!("ignoring windows echo: level={} (we set it {:?} ago)", level, t.elapsed());
                return None;
            }
        }
        if Some(level) == inner.last_level {
            return None;
        }
        inner.last_pushed_to_speaker = Some((level, Instant::now()));
        inner.last_level = Some(level);
        Some(level)
    }

    /// The speaker reported a new volume (`0..=100`, via GENA NOTIFY).
    /// Returns the level to set on the Windows slider, or `None` if it's
    /// an echo / no change.
    pub fn sonos_changed(&self, level: u32) -> Option<u32> {
        let level = level.min(100);
        let mut inner = self.inner.lock().unwrap();
        if let Some((lvl, t)) = inner.last_pushed_to_speaker {
            if t.elapsed() < ECHO_IGNORE_WINDOW && lvl == level {
                debug!("ignoring speaker echo: level={} (we set it {:?} ago)", level, t.elapsed());
                return None;
            }
        }
        if Some(level) == inner.last_level {
            return None;
        }
        inner.last_pushed_to_windows = Some((level, Instant::now()));
        inner.last_level = Some(level);
        Some(level)
    }
}

impl VolumeSync {
    /// Last-known volume level (`0..=100`), set by either `*_changed`
    /// path or by `prime_initial_volume`. The GUI reads this to render
    /// the volume slider (m24).
    pub fn current_level(&self) -> Option<u32> {
        self.inner.lock().unwrap().last_level
    }

    /// Seed the cache from an initial observation (e.g. `upnp::get_volume`
    /// on connect, or the GUI slider). Doesn't touch the `last_pushed_*`
    /// echo timestamps — the caller observed/initiated this directly.
    pub fn prime_initial_volume(&self, level: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.last_level = Some(level.min(100));
    }
}

impl Default for VolumeSync {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert Windows millibels (`-10000..=0`) to a `0..=100` level.
///
/// **Fallback only** — used when the exact slider scalar can't be read
/// from Core Audio. This is an approximation of Windows' (undocumented,
/// version-dependent) volume taper, not an exact inverse of it.
pub fn millibels_to_sonos(mb: i32) -> u32 {
    if mb >= 0 {
        return 100;
    }
    if mb <= -10000 {
        return 0;
    }
    // mb is in millibels; convert to dB then to a linear ratio.
    // ratio = 10^(mb / 5000); level = round(100 * ratio).
    let exp = (mb as f64) / 5000.0;
    let ratio = (10f64).powf(exp);
    let level = (100.0 * ratio).round().clamp(0.0, 100.0);
    level as u32
}

/// Inverse of `millibels_to_sonos`. **Fallback only** (see above).
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
            assert!(
                (back as i32 - level as i32).abs() <= 1,
                "level {} -> {} -> {}",
                level,
                mb,
                back
            );
        }
        assert_eq!(millibels_to_sonos(0), 100);
        assert_eq!(millibels_to_sonos(-10000), 0);
    }

    #[test]
    fn speaker_echo_is_ignored() {
        let s = VolumeSync::new();
        // Speaker tells us volume=50; we forward it to the Windows slider.
        assert_eq!(s.sonos_changed(50), Some(50));
        // The driver immediately echoes the new level back. Dropped.
        assert_eq!(s.driver_changed(50), None);
    }

    #[test]
    fn windows_echo_is_ignored() {
        let s = VolumeSync::new();
        // Windows slider moves to 30; we forward it to the speaker.
        assert_eq!(s.driver_changed(30), Some(30));
        // The speaker's GENA NOTIFY echoes 30 back. Dropped.
        assert_eq!(s.sonos_changed(30), None);
    }

    #[test]
    fn distinct_changes_pass_through() {
        let s = VolumeSync::new();
        assert_eq!(s.driver_changed(40), Some(40));
        // A genuine, different speaker-side change is forwarded.
        assert_eq!(s.sonos_changed(70), Some(70));
        // Re-sending the same level is a no-op.
        assert_eq!(s.sonos_changed(70), None);
    }

    #[test]
    fn level_is_clamped() {
        let s = VolumeSync::new();
        assert_eq!(s.driver_changed(150), Some(100));
    }
}
