//! Bidirectional volume sync between the Windows endpoint slider and the
//! speaker, with echo suppression so a change made on one side doesn't
//! bounce back from the other.
//!
//! Both sides speak the same linear `0..=100` scale:
//!
//!  * Windows: our driver exposes a hardware volume node advertising a
//!    −96..0 dB range (`KSPROPERTY_AUDIO_VOLUMELEVEL`). For a hardware
//!    volume node Windows maps the slider **linearly across that dB
//!    range** — slider 50 % lands on −48 dB, the midpoint, *not* on the
//!    perceptual ("audio-tapered") curve that `IAudioEndpointVolume`'s
//!    scalar methods use. So we receive the dB over the IOCTL and convert
//!    it back to a percent **linearly**; that recovers the slider
//!    position 1:1.
//!  * Speaker (Sonos/UPnP): the `RenderingControl` `Volume` is already a
//!    `0..=100` value matching the speaker's own UI slider.
//!
//! Earlier this used a logarithmic `100 * 10^(mb/5000)` conversion, on
//! the assumption that Windows applied a perceptual taper to the dB. It
//! doesn't (for a hardware node), so that double-counted the curve and
//! made e.g. a 50 % slider show as ~11 % on the speaker. The mapping is
//! plain linear.

use log::debug;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Window inside which we ignore "echoes" of changes we just made.
const ECHO_IGNORE_WINDOW: Duration = Duration::from_millis(250);

/// Bottom of the dB range our driver's volume node advertises, in
/// millibels. MUST match `STREAM_TO_SPEAKER_VOLUME_MIN_MILLIBELS` in
/// `driver/driver.h` (−9600 = −96 dB) — Windows maps the slider linearly
/// across `[VOLUME_MIN_MILLIBELS, 0]`, so this is the 0 % anchor.
const VOLUME_MIN_MILLIBELS: i32 = -9600;

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

    /// The Windows slider moved (reported via the driver event). `level`
    /// is `0..=100`. Returns the level to push to the speaker, or `None`
    /// if it's an echo / no change.
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

/// Convert a Windows millibel value from our volume node to a `0..=100`
/// level. Linear across `[VOLUME_MIN_MILLIBELS, 0]` — see the module
/// docs for why this isn't logarithmic.
pub fn millibels_to_sonos(mb: i32) -> u32 {
    if mb >= 0 {
        return 100;
    }
    if mb <= VOLUME_MIN_MILLIBELS {
        return 0;
    }
    let frac = (mb - VOLUME_MIN_MILLIBELS) as f64 / (-VOLUME_MIN_MILLIBELS) as f64;
    (frac * 100.0).round().clamp(0.0, 100.0) as u32
}

/// Inverse of `millibels_to_sonos`: a `0..=100` level to the millibel
/// value to set on our node so the Windows slider shows that percent.
pub fn sonos_to_millibels(level: u32) -> i32 {
    let level = level.min(100);
    if level == 0 {
        return VOLUME_MIN_MILLIBELS;
    }
    if level >= 100 {
        return 0;
    }
    let mb = VOLUME_MIN_MILLIBELS as f64 * (1.0 - level as f64 / 100.0);
    mb.round() as i32
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
        assert_eq!(millibels_to_sonos(VOLUME_MIN_MILLIBELS), 0);
        assert_eq!(millibels_to_sonos(-10000), 0); // clamps below the range
    }

    #[test]
    fn conversion_is_linear() {
        // The whole point of the fix: slider % maps 1:1 through the dB.
        // These are the midpoints Windows produces for our −9600..0 node.
        assert_eq!(millibels_to_sonos(-4800), 50);
        assert_eq!(millibels_to_sonos(-2400), 75);
        assert_eq!(millibels_to_sonos(-7680), 20);
        assert_eq!(sonos_to_millibels(50), -4800);
        assert_eq!(sonos_to_millibels(25), -7200);
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
