//! Ten-band graphic equalizer applied to the shared PCM stream.
//!
//! Every output (UPnP HTTP, RAOP, AirPlay 2) reads the same hub, so the
//! audio loop runs this once per packet before publishing. Design:
//!
//! - Ten RBJ-cookbook peaking biquads (Q = 1.4) at octave centres from
//!   31.25 Hz to 16 kHz, gain −12..+12 dB in 0.5 dB steps, plus a preamp
//!   in the same range.
//! - **Automatic headroom**: the cascade's magnitude response is
//!   evaluated at DC, Nyquist, every band centre and 4096 log-spaced
//!   points from 10 Hz to Nyquist; the peak (floored at 0 dB) plus the
//!   preamp, when positive, is taken back off as extra attenuation. A
//!   boosted curve therefore never pushes a full-scale tone into the
//!   clamp in steady state.
//! - Disabled, or flat with no net preamp: **bit-exact bypass**, no
//!   per-sample work at all.
//! - **Click-free changes**: a new curve runs alongside the old one and
//!   the outputs are crossfaded linearly over 20 ms. Updates arriving
//!   mid-fade are held (latest wins) until the fade ends; an identical
//!   curve keeps the running filter state.
//! - Designs are computed on a short-lived worker thread (never the GUI
//!   or audio thread); the audio thread picks the newest one up with a
//!   non-blocking poll.
//!
//! Processing is in f64 (transposed direct form II) on samples scaled to
//! ±1.0; the result is rounded to nearest and clamped back to i16.
//! Rounding error is ~−98 dBFS, well under the 16-bit floor that the
//! stream already carries, and digital silence stays exactly zero.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Number of bands.
pub const EQ_BANDS: usize = 10;
/// Band centre frequencies, Hz.
pub const EQ_BAND_HZ: [f64; EQ_BANDS] =
    [31.25, 62.5, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
/// Quality factor shared by every band (~one octave wide).
pub const EQ_Q: f64 = 1.4;
/// Per-band gain and preamp limits, dB.
pub const EQ_GAIN_MIN_DB: f32 = -12.0;
pub const EQ_GAIN_MAX_DB: f32 = 12.0;
/// Gain resolution, dB.
pub const EQ_GAIN_STEP_DB: f32 = 0.5;
/// Crossfade length between two curves, as a fraction of a second
/// (rate / 50 = 20 ms).
const FADE_DIVISOR: u32 = 50;
/// Log-spaced evaluation points for the headroom peak search.
const RESPONSE_POINTS: usize = 4096;
/// Lowest frequency of the headroom peak search, Hz.
const RESPONSE_MIN_HZ: f64 = 10.0;
/// Minimum spacing between two designs reaching the audio thread, so a
/// dragged slider doesn't restart the crossfade every frame.
const DESIGN_THROTTLE: Duration = Duration::from_millis(50);

/// Persisted equalizer curve. `enabled = false` keeps the curve so
/// turning it back on restores it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EqSettings {
    pub enabled: bool,
    pub preamp_db: f32,
    pub bands_db: [f32; EQ_BANDS],
}

impl Default for EqSettings {
    fn default() -> Self {
        Self { enabled: false, preamp_db: 0.0, bands_db: [0.0; EQ_BANDS] }
    }
}

/// Clamp to the supported range and snap to the 0.5 dB grid. Non-finite
/// values (a hand-edited config) become 0.
fn snap_db(v: f32) -> f32 {
    if !v.is_finite() {
        return 0.0;
    }
    let v = v.clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB);
    let snapped = (v / EQ_GAIN_STEP_DB).round() * EQ_GAIN_STEP_DB;
    // Normalise -0.0 so flat comparisons and presets match exactly.
    if snapped == 0.0 { 0.0 } else { snapped }
}

impl EqSettings {
    /// Copy with every gain clamped and snapped to the supported grid.
    pub fn sanitized(&self) -> Self {
        let mut s = self.clone();
        s.preamp_db = snap_db(s.preamp_db);
        for b in s.bands_db.iter_mut() {
            *b = snap_db(*b);
        }
        s
    }

    /// All bands at 0 dB (the preamp is not a band).
    pub fn bands_flat(&self) -> bool {
        self.bands_db.iter().all(|&g| g == 0.0)
    }
}

/// Lenient field deserializer for `UserConfig`: a malformed equalizer
/// entry falls back to the default curve instead of failing the whole
/// config file (which would quarantine everything else in it).
pub fn deserialize_lenient<'de, D>(d: D) -> Result<EqSettings, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match serde_json::from_value::<EqSettings>(v) {
        Ok(s) => s.sanitized(),
        Err(e) => {
            log::warn!("user_config: ignoring malformed equalizer settings ({e})");
            EqSettings::default()
        }
    })
}

/// A named curve offered in the editor. Selecting one sets the bands and
/// resets the preamp to 0.
pub struct EqPreset {
    pub name: &'static str,
    pub bands_db: [f32; EQ_BANDS],
}

pub const EQ_PRESETS: &[EqPreset] = &[
    EqPreset { name: "Flat", bands_db: [0.0; EQ_BANDS] },
    EqPreset {
        name: "Bass boost",
        bands_db: [6.0, 5.0, 3.5, 1.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    EqPreset {
        name: "Bass cut",
        bands_db: [-6.0, -5.0, -3.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    EqPreset {
        name: "Treble boost",
        bands_db: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 3.0, 4.5, 5.5],
    },
    EqPreset {
        name: "Vocal",
        bands_db: [-2.0, -1.5, -1.0, 0.0, 1.5, 3.0, 3.5, 2.5, 1.0, 0.0],
    },
    EqPreset {
        name: "Loudness",
        bands_db: [5.0, 4.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.5, 3.0, 4.0],
    },
    EqPreset {
        name: "Small speaker",
        bands_db: [-6.0, -3.0, 1.5, 2.5, 1.0, 0.0, 0.0, 1.0, 2.0, 1.5],
    },
];

/// Name of the preset whose bands match exactly, or `None` (= custom).
pub fn matching_preset(bands_db: &[f32; EQ_BANDS]) -> Option<&'static str> {
    EQ_PRESETS.iter().find(|p| &p.bands_db == bands_db).map(|p| p.name)
}

/// One normalised biquad section (a0 = 1).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Biquad {
    /// RBJ Audio-EQ-Cookbook peaking filter.
    fn peaking(sample_rate: f64, f0: f64, q: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f0 / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cos) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha / a) / a0,
        }
    }

    /// |H(e^{jw})| at angular frequency `w` (radians/sample).
    fn magnitude(&self, w: f64) -> f64 {
        // z^-1 = e^{-jw}, z^-2 = e^{-2jw}.
        let (s1, c1) = (-w).sin_cos();
        let (s2, c2) = (-2.0 * w).sin_cos();
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = self.b1 * s1 + self.b2 * s2;
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = self.a1 * s1 + self.a2 * s2;
        ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)).sqrt()
    }
}

/// A prepared curve: filter coefficients plus the output gain after
/// automatic headroom. Built off the audio thread by [`EqDesign::new`].
#[derive(Clone, Debug)]
pub struct EqDesign {
    /// The (sanitised) settings this was built from.
    pub settings: EqSettings,
    sections: Vec<Biquad>,
    /// Linear output gain = effective preamp.
    gain: f64,
    /// Peak of the bands' combined magnitude response, dB (≥ 0).
    pub peak_db: f32,
    /// Automatic attenuation applied on top of the preamp, dB (≤ 0).
    pub auto_attenuation_db: f32,
    /// Preamp actually applied = preamp + automatic attenuation, dB.
    pub effective_preamp_db: f32,
    bypass: bool,
}

impl EqDesign {
    pub fn new(settings: &EqSettings, sample_rate: u32) -> Self {
        let settings = settings.sanitized();
        let fs = sample_rate as f64;
        let nyquist = fs / 2.0;
        let sections: Vec<Biquad> = EQ_BAND_HZ
            .iter()
            .zip(settings.bands_db.iter())
            .filter(|(&f0, &g)| g != 0.0 && f0 < nyquist)
            .map(|(&f0, &g)| Biquad::peaking(fs, f0, EQ_Q, g as f64))
            .collect();

        let peak_db = if sections.is_empty() {
            0.0
        } else {
            let response_db = |hz: f64| -> f64 {
                let w = 2.0 * std::f64::consts::PI * hz / fs;
                let mag: f64 = sections.iter().map(|s| s.magnitude(w)).product();
                20.0 * mag.max(1e-12).log10()
            };
            let mut peak = response_db(0.0).max(response_db(nyquist));
            for &f0 in EQ_BAND_HZ.iter().filter(|&&f| f < nyquist) {
                peak = peak.max(response_db(f0));
            }
            let ratio = (nyquist / RESPONSE_MIN_HZ).ln();
            for i in 0..RESPONSE_POINTS {
                let t = i as f64 / (RESPONSE_POINTS - 1) as f64;
                peak = peak.max(response_db(RESPONSE_MIN_HZ * (ratio * t).exp()));
            }
            peak.max(0.0)
        };

        let preamp = settings.preamp_db as f64;
        let attenuation = -(preamp + peak_db).max(0.0);
        let effective = preamp + attenuation;
        let bypass = !settings.enabled || (sections.is_empty() && effective == 0.0);
        Self {
            gain: 10f64.powf(effective / 20.0),
            peak_db: peak_db as f32,
            auto_attenuation_db: attenuation as f32,
            effective_preamp_db: effective as f32,
            bypass,
            sections,
            settings,
        }
    }

    /// The pass-through design the audio loop starts with.
    pub fn bypass(sample_rate: u32) -> Self {
        Self::new(&EqSettings::default(), sample_rate)
    }

    pub fn is_bypass(&self) -> bool {
        self.bypass
    }
}

/// Running filter state for one design.
struct Chain {
    design: Arc<EqDesign>,
    /// Transposed-DF-II state, `[z1, z2]` per section per channel,
    /// laid out section-major.
    state: Vec<[f64; 2]>,
}

impl Chain {
    fn new(design: Arc<EqDesign>, channels: usize) -> Self {
        let n = design.sections.len() * channels;
        Self { design, state: vec![[0.0; 2]; n] }
    }

    #[inline]
    fn process(&mut self, x: f64, ch: usize, channels: usize) -> f64 {
        if self.design.bypass {
            return x;
        }
        let mut y = x;
        for (i, s) in self.design.sections.iter().enumerate() {
            let z = &mut self.state[i * channels + ch];
            let out = s.b0 * y + z[0];
            z[0] = s.b1 * y - s.a1 * out + z[1];
            z[1] = s.b2 * y - s.a2 * out;
            y = out;
        }
        y * self.design.gain
    }

    /// Flush denormal-range state to zero after a quiet stretch so the
    /// FPU never sits in the slow subnormal path.
    fn flush_tiny(&mut self) {
        for z in self.state.iter_mut() {
            for v in z.iter_mut() {
                if v.abs() < 1e-25 {
                    *v = 0.0;
                }
            }
        }
    }
}

/// Audio-thread equalizer: the active chain, an optional chain being
/// faded out, and at most one queued design.
pub struct Equalizer {
    channels: usize,
    fade_len: usize,
    current: Chain,
    fading_out: Option<Chain>,
    fade_pos: usize,
    queued: Option<Arc<EqDesign>>,
}

impl Equalizer {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let channels = channels.max(1) as usize;
        Self {
            channels,
            fade_len: (sample_rate / FADE_DIVISOR).max(1) as usize,
            current: Chain::new(Arc::new(EqDesign::bypass(sample_rate)), channels),
            fading_out: None,
            fade_pos: 0,
            queued: None,
        }
    }

    /// Switch to `design`, crossfading from the current curve. Identical
    /// settings are ignored (filter state kept); during a fade the design
    /// waits, replacing any earlier waiting one.
    pub fn set_design(&mut self, design: Arc<EqDesign>) {
        if self.fading_out.is_some() {
            self.queued = Some(design);
            return;
        }
        if design.settings == self.current.design.settings
            || (design.bypass && self.current.design.bypass)
        {
            return;
        }
        let next = Chain::new(design, self.channels);
        self.fading_out = Some(std::mem::replace(&mut self.current, next));
        self.fade_pos = 0;
    }

    /// True when processing is a no-op (bypassed, nothing fading).
    pub fn is_idle(&self) -> bool {
        self.fading_out.is_none() && self.current.design.bypass
    }

    /// Process interleaved samples in place.
    pub fn process(&mut self, samples: &mut [i16]) {
        if self.is_idle() {
            return;
        }
        let ch = self.channels;
        for frame in samples.chunks_mut(ch) {
            let t = if self.fading_out.is_some() {
                (self.fade_pos as f64 + 1.0) / self.fade_len as f64
            } else {
                1.0
            };
            for (c, s) in frame.iter_mut().enumerate() {
                let x = *s as f64 / 32768.0;
                let mut y = self.current.process(x, c, ch);
                if let Some(old) = self.fading_out.as_mut() {
                    let y_old = old.process(x, c, ch);
                    y = y_old + (y - y_old) * t;
                }
                *s = (y * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
            }
            if self.fading_out.is_some() {
                self.fade_pos += 1;
                if self.fade_pos >= self.fade_len {
                    self.fading_out = None;
                    if let Some(next) = self.queued.take() {
                        self.set_design(next);
                    }
                }
            }
        }
        self.current.flush_tiny();
        if let Some(old) = self.fading_out.as_mut() {
            old.flush_tiny();
        }
    }
}

/// What the editor shows next to the preamp: the automatic attenuation
/// and effective preamp for a given curve.
#[derive(Clone, Debug, PartialEq)]
pub struct EqReport {
    pub settings: EqSettings,
    pub auto_attenuation_db: f32,
    pub effective_preamp_db: f32,
}

/// Hand-off between whoever edits the curve (GUI, startup) and the audio
/// thread. `request` never blocks on DSP work: designs are built on a
/// worker thread that exists only while requests are pending.
pub struct EqControl {
    sample_rate: u32,
    request: Mutex<Option<EqSettings>>,
    worker_running: AtomicBool,
    ready: Mutex<Option<Arc<EqDesign>>>,
    ready_flag: AtomicBool,
    report: Mutex<Option<EqReport>>,
}

impl EqControl {
    pub fn new(sample_rate: u32) -> Arc<Self> {
        Arc::new(Self {
            sample_rate,
            request: Mutex::new(None),
            worker_running: AtomicBool::new(false),
            ready: Mutex::new(None),
            ready_flag: AtomicBool::new(false),
            report: Mutex::new(None),
        })
    }

    /// Ask for `settings` to be applied to the stream. Only the latest
    /// pending request is kept.
    pub fn request(self: &Arc<Self>, settings: EqSettings) {
        *self.request.lock().unwrap() = Some(settings.sanitized());
        if self.worker_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("eq-design".into())
            .spawn(move || me.design_worker());
        if let Err(e) = spawned {
            // No thread: build it inline rather than drop the change.
            log::warn!("equalizer: can't spawn design thread ({e}); designing inline");
            self.design_pending();
            self.worker_running.store(false, Ordering::Release);
        }
    }

    fn design_worker(&self) {
        let mut last_publish: Option<Instant> = None;
        loop {
            if let Some(at) = last_publish {
                let since = at.elapsed();
                if since < DESIGN_THROTTLE {
                    std::thread::sleep(DESIGN_THROTTLE - since);
                }
            }
            if self.design_pending() {
                last_publish = Some(Instant::now());
                continue;
            }
            self.worker_running.store(false, Ordering::Release);
            // A request that landed between the empty check and the flag
            // store would otherwise wait for the next one.
            if self.request.lock().unwrap().is_none()
                || self.worker_running.swap(true, Ordering::AcqRel)
            {
                return;
            }
        }
    }

    /// Build the pending request, if any. Returns whether one was built.
    fn design_pending(&self) -> bool {
        let Some(settings) = self.request.lock().unwrap().take() else {
            return false;
        };
        let design = Arc::new(EqDesign::new(&settings, self.sample_rate));
        *self.report.lock().unwrap() = Some(EqReport {
            settings: design.settings.clone(),
            auto_attenuation_db: design.auto_attenuation_db,
            effective_preamp_db: design.effective_preamp_db,
        });
        *self.ready.lock().unwrap() = Some(design);
        self.ready_flag.store(true, Ordering::Release);
        true
    }

    /// Audio thread: the newest finished design, if one arrived since the
    /// last call. Never blocks — a contended lock just means "next packet".
    pub fn take_ready(&self) -> Option<Arc<EqDesign>> {
        if !self.ready_flag.load(Ordering::Acquire) {
            return None;
        }
        let mut slot = self.ready.try_lock().ok()?;
        self.ready_flag.store(false, Ordering::Release);
        slot.take()
    }

    /// The most recently designed curve's headroom figures.
    pub fn report(&self) -> Option<EqReport> {
        self.report.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 44_100;

    fn sine(freq: f64, amp: f64, frames: usize) -> Vec<i16> {
        let mut v = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let x = (2.0 * std::f64::consts::PI * freq * n as f64 / FS as f64).sin() * amp;
            let s = (x * 32767.0).round() as i16;
            v.push(s);
            v.push(s);
        }
        v
    }

    fn settled(eq: &mut Equalizer, design: EqDesign) {
        eq.set_design(Arc::new(design));
        // Run the crossfade out on silence.
        let mut pad = vec![0i16; (FS as usize / 10) * 2];
        eq.process(&mut pad);
        assert!(eq.fading_out.is_none());
    }

    #[test]
    fn disabled_and_flat_are_bit_exact_bypass() {
        let mut input = sine(997.0, 0.9, 4410);
        input[10] = i16::MIN;
        input[11] = i16::MAX;
        for s in [
            EqSettings { enabled: false, preamp_db: 6.0, bands_db: [12.0; EQ_BANDS] },
            EqSettings { enabled: true, preamp_db: 0.0, bands_db: [0.0; EQ_BANDS] },
            // Positive preamp on a flat curve is fully taken back by headroom.
            EqSettings { enabled: true, preamp_db: 9.0, bands_db: [0.0; EQ_BANDS] },
        ] {
            let d = EqDesign::new(&s, FS);
            assert!(d.is_bypass(), "{s:?}");
            let mut eq = Equalizer::new(FS, 2);
            eq.set_design(Arc::new(d));
            assert!(eq.is_idle());
            let mut buf = input.clone();
            eq.process(&mut buf);
            assert_eq!(buf, input);
        }
    }

    #[test]
    fn flat_response_is_unity_and_negative_preamp_is_plain_gain() {
        let d = EqDesign::new(
            &EqSettings { enabled: true, preamp_db: -6.0, bands_db: [0.0; EQ_BANDS] },
            FS,
        );
        assert!(!d.is_bypass());
        assert_eq!(d.auto_attenuation_db, 0.0);
        assert_eq!(d.effective_preamp_db, -6.0);
        let mut eq = Equalizer::new(FS, 2);
        settled(&mut eq, d);
        let mut buf = vec![20000i16, -20000, 1000, -1000];
        eq.process(&mut buf);
        let g = 10f64.powf(-6.0 / 20.0);
        assert_eq!(buf[0], (20000.0 * g).round() as i16);
        assert_eq!(buf[1], (-20000.0 * g).round() as i16);
    }

    #[test]
    fn peaking_section_hits_its_gain_at_centre_and_unity_far_away() {
        let b = Biquad::peaking(FS as f64, 1000.0, EQ_Q, 12.0);
        let w = |hz: f64| 2.0 * std::f64::consts::PI * hz / FS as f64;
        let db = |hz: f64| 20.0 * b.magnitude(w(hz)).log10();
        assert!((db(1000.0) - 12.0).abs() < 1e-9);
        assert!(db(20.0).abs() < 0.05);
        assert!(db(20000.0).abs() < 0.2);
    }

    #[test]
    fn headroom_matches_peak_and_prevents_clipping_full_scale_sine() {
        let mut bands = [0.0f32; EQ_BANDS];
        bands[5] = 12.0; // 1 kHz
        let s = EqSettings { enabled: true, preamp_db: 3.0, bands_db: bands };
        let d = EqDesign::new(&s, FS);
        // A lone band's peak is its own gain; preamp + peak comes back off.
        assert!((d.peak_db - 12.0).abs() < 0.01, "peak {}", d.peak_db);
        assert!((d.auto_attenuation_db + 15.0).abs() < 0.01);
        assert!((d.effective_preamp_db + 12.0).abs() < 0.01);

        // Full-scale 1 kHz sine through the chain in float (before the
        // i16 clamp could hide an overshoot): steady state stays within
        // full scale, and lands back at ~0 dB rather than over-attenuated.
        let d = Arc::new(d);
        let mut chain = Chain::new(Arc::clone(&d), 1);
        let mut peak = 0f64;
        for n in 0..FS as usize {
            let x = (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / FS as f64).sin();
            let y = chain.process(x, 0, 1);
            if n > FS as usize / 2 {
                peak = peak.max(y.abs());
            }
        }
        assert!(peak <= 1.0 + 1e-6, "would clip: {peak}");
        assert!(peak > 0.99, "headroom over-attenuated: {peak}");

        // And through the i16 path: no sample pinned at either rail.
        let mut eq = Equalizer::new(FS, 2);
        settled(&mut eq, (*d).clone());
        let mut buf = sine(1000.0, 0.99, FS as usize);
        eq.process(&mut buf);
        let tail = &buf[(FS as usize / 2) * 2..];
        assert!(tail.iter().all(|&s| s > i16::MIN && s < i16::MAX));
    }

    #[test]
    fn overlapping_boosts_are_measured_on_the_cascade() {
        let s = EqSettings { enabled: true, preamp_db: 0.0, bands_db: [12.0; EQ_BANDS] };
        let d = EqDesign::new(&s, FS);
        // Neighbouring octave bands overlap, so the summed peak exceeds
        // any single band's 12 dB.
        assert!(d.peak_db > 12.0);
        let mut eq = Equalizer::new(FS, 2);
        settled(&mut eq, d);
        for hz in [62.5, 440.0, 3000.0, 12000.0] {
            let mut buf = sine(hz, 1.0, FS as usize / 2);
            eq.process(&mut buf);
            let tail = &buf[(FS as usize / 4) * 2..];
            let peak = tail.iter().map(|&s| (s as i32).abs()).max().unwrap();
            assert!(peak < 32767, "{hz} Hz clipped");
        }
    }

    #[test]
    fn crossfade_is_continuous_and_identical_settings_keep_state() {
        let mut bands = [0.0f32; EQ_BANDS];
        bands[3] = -12.0;
        let a = EqSettings { enabled: true, preamp_db: 0.0, bands_db: bands };
        let mut eq = Equalizer::new(FS, 2);
        settled(&mut eq, EqDesign::new(&a, FS));
        let mut warm = sine(250.0, 0.5, 4410);
        eq.process(&mut warm);
        let state_before = eq.current.state.clone();
        // Same settings: no fade, state untouched.
        eq.set_design(Arc::new(EqDesign::new(&a, FS)));
        assert!(eq.fading_out.is_none());
        assert_eq!(eq.current.state, state_before);

        // Switching to bypass mid-tone fades over 20 ms: no sample-to-
        // sample jump bigger than the tone's own slope allows.
        eq.set_design(Arc::new(EqDesign::bypass(FS)));
        assert!(eq.fading_out.is_some());
        let mut buf = sine(250.0, 0.5, 4410);
        eq.process(&mut buf);
        let max_step = buf
            .chunks(2)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1][0] as i32 - w[0][0] as i32).abs())
            .max()
            .unwrap();
        // 250 Hz at half scale moves at most ~584 per sample.
        assert!(max_step < 700, "step {max_step}");
        assert!(eq.is_idle(), "fade finished into bypass");
    }

    #[test]
    fn updates_during_a_fade_queue_only_the_latest() {
        let mut eq = Equalizer::new(FS, 2);
        let mk = |g: f32| {
            let mut b = [0.0f32; EQ_BANDS];
            b[0] = g;
            Arc::new(EqDesign::new(&EqSettings { enabled: true, preamp_db: 0.0, bands_db: b }, FS))
        };
        eq.set_design(mk(3.0));
        eq.set_design(mk(4.0));
        eq.set_design(mk(5.0));
        assert_eq!(eq.queued.as_ref().unwrap().settings.bands_db[0], 5.0);
        let mut pad = vec![0i16; 882 * 2];
        eq.process(&mut pad);
        // First fade done → the queued one started its own fade.
        assert_eq!(eq.current.design.settings.bands_db[0], 5.0);
        assert!(eq.queued.is_none());
    }

    #[test]
    fn settings_are_snapped_and_presets_recognised() {
        let s = EqSettings {
            enabled: true,
            preamp_db: f32::NAN,
            bands_db: [0.26, -0.24, 30.0, -30.0, 1.74, 0.0, 0.0, 0.0, 0.0, -0.1],
        }
        .sanitized();
        assert_eq!(s.preamp_db, 0.0);
        assert_eq!(&s.bands_db[..5], &[0.5, 0.0, 12.0, -12.0, 1.5]);
        assert_eq!(s.bands_db[9].to_bits(), 0.0f32.to_bits(), "no negative zero");
        for p in EQ_PRESETS {
            assert_eq!(matching_preset(&p.bands_db), Some(p.name));
            for &g in &p.bands_db {
                assert_eq!(snap_db(g), g, "{} not on the grid", p.name);
            }
        }
        assert_eq!(matching_preset(&[1.0; EQ_BANDS]), None);
    }

    #[test]
    fn control_hands_designs_to_the_audio_side() {
        let ctl = EqControl::new(FS);
        let mut b = [0.0f32; EQ_BANDS];
        b[9] = 6.0;
        ctl.request(EqSettings { enabled: true, preamp_db: 0.0, bands_db: b });
        let start = Instant::now();
        let d = loop {
            if let Some(d) = ctl.take_ready() {
                break d;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "design never arrived");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(d.settings.bands_db[9], 6.0);
        let r = ctl.report().unwrap();
        assert!(r.auto_attenuation_db < -5.0);
        assert!(ctl.take_ready().is_none(), "taken once");
    }
}
