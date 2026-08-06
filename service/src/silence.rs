//! Silence detection + low-noise injection.
//!
//! Sonos drops the HTTP stream if we send a long run of literal zeros (it
//! decides the source died). To keep the connection alive during silence,
//! we replace fully-silent packets with a tiny white-noise floor at peak
//! ~|4| (well below -78 dBFS) once we have been silent for `silent_threshold`
//! packets in a row.

use crate::audio_source::AudioPacket;

/// Peak amplitude considered "silent". -90 dBFS on i16 ~= |x| < 7.
const SILENCE_PEAK_THRESHOLD: i16 = 7;

/// Default consecutive-silent-packets count before we declare quiescence
/// (~500 ms at 10 ms packets).
pub const DEFAULT_QUIESCENT_AFTER_PACKETS: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilenceState {
    Active,
    Quiescent,
}

/// Silence-detection state machine. Owns the LCG it uses to generate the
/// noise floor so the implementation is deterministic for tests.
pub struct SilenceDetector {
    state: SilenceState,
    consecutive_silent_packets: u32,
    quiescent_after: u32,
    inject_noise: bool,
    rng_state: u32,
}

impl SilenceDetector {
    pub fn new(quiescent_after_packets: u32, inject_noise: bool) -> Self {
        Self {
            state: SilenceState::Active,
            consecutive_silent_packets: 0,
            quiescent_after: quiescent_after_packets.max(1),
            inject_noise,
            // Arbitrary non-zero seed.
            rng_state: 0x9E37_79B9,
        }
    }

    /// Current state. Mainly for tests / logging.
    pub fn state(&self) -> SilenceState {
        self.state
    }

    /// Inspect a packet and (if we're quiescent) rewrite the samples in
    /// place with low-amplitude noise. Returns the current state after the
    /// update.
    pub fn process(&mut self, pkt: &mut AudioPacket) -> SilenceState {
        let peak = peak_abs_i16(&pkt.samples);

        if peak > SILENCE_PEAK_THRESHOLD {
            // Real audio. Immediately back to Active.
            self.state = SilenceState::Active;
            self.consecutive_silent_packets = 0;
        } else {
            // Silent packet.
            self.consecutive_silent_packets =
                self.consecutive_silent_packets.saturating_add(1);
            if self.consecutive_silent_packets >= self.quiescent_after {
                self.state = SilenceState::Quiescent;
            }
        }

        if self.state == SilenceState::Quiescent && self.inject_noise {
            self.fill_with_noise(&mut pkt.samples);
        }

        self.state
    }

    /// Fill `samples` with a deterministic low-amplitude pseudo-random
    /// dither (peak ~|4|, well below -78 dBFS).
    fn fill_with_noise(&mut self, samples: &mut [i16]) {
        // Tiny LCG; we only need ~3 bits of randomness per sample.
        for s in samples.iter_mut() {
            // Linear congruential — Numerical Recipes constants.
            self.rng_state = self
                .rng_state
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            // Take the top 3 bits, sign-extended to [-4..3].
            let n = ((self.rng_state >> 29) as i16) - 4;
            *s = n;
        }
    }
}

/// Scan an i16 slice for the absolute peak.
///
/// The "block-of-4 reinterpret" trick: we view runs of four i16 values as a
/// single u64, then mask the sign bits and take the max. On modern CPUs the
/// scalar version below already vectorizes; the unsafe variant exists more
/// to give the compiler aligned 64-bit chunks. The unit tests below compare
/// it against a naive loop.
fn peak_abs_i16(samples: &[i16]) -> i16 {
    if samples.is_empty() {
        return 0;
    }

    let mut peak: i32 = 0;
    let len = samples.len();
    let block_count = len / 4;
    let remainder_start = block_count * 4;

    // SAFETY: We bounds-check `block_count * 4 <= len` above, and `read`s
    // happen from the original slice's pointer with the original lifetime.
    // Pointer-to-pointer cast from &[i16] to *const u64 is safe because the
    // alignment of i16 (2) is less than u64 (8), so we use unaligned reads.
    let ptr = samples.as_ptr();
    for b in 0..block_count {
        // SAFETY: see above.
        let chunk = unsafe { (ptr.add(b * 4) as *const u64).read_unaligned() };
        // Pull out four signed shorts.
        let s0 = (chunk & 0xFFFF) as u16 as i16;
        let s1 = ((chunk >> 16) & 0xFFFF) as u16 as i16;
        let s2 = ((chunk >> 32) & 0xFFFF) as u16 as i16;
        let s3 = ((chunk >> 48) & 0xFFFF) as u16 as i16;
        // abs() in i32 to keep i16::MIN safe.
        let a0 = (s0 as i32).unsigned_abs() as i32;
        let a1 = (s1 as i32).unsigned_abs() as i32;
        let a2 = (s2 as i32).unsigned_abs() as i32;
        let a3 = (s3 as i32).unsigned_abs() as i32;
        let m = a0.max(a1).max(a2.max(a3));
        if m > peak {
            peak = m;
        }
    }
    for s in &samples[remainder_start..] {
        let a = (*s as i32).unsigned_abs() as i32;
        if a > peak {
            peak = a;
        }
    }
    peak.min(i16::MAX as i32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_source::AudioPacket;

    fn silent_packet(n_frames: usize) -> AudioPacket {
        AudioPacket {
            samples: vec![0i16; n_frames * 2],
            sample_rate: 44_100,
            channels: 2,
            timestamp_qpc: 0,
            stream_position: 0,
            flags: 0,
        }
    }

    fn loud_packet(n_frames: usize) -> AudioPacket {
        AudioPacket {
            samples: vec![10_000i16; n_frames * 2],
            sample_rate: 44_100,
            channels: 2,
            timestamp_qpc: 0,
            stream_position: 0,
            flags: 0,
        }
    }

    #[test]
    fn peak_scalar_matches_block() {
        let mut v: Vec<i16> = (0..1000).map(|i| ((i % 200) as i16 - 50) * 3).collect();
        let p1 = peak_abs_i16(&v);
        let p2 = v
            .iter()
            .map(|s| (*s as i32).unsigned_abs() as i32)
            .max()
            .unwrap_or(0) as i16;
        assert_eq!(p1, p2);

        v.push(-32_768);
        let p3 = peak_abs_i16(&v);
        assert_eq!(p3, i16::MAX); // abs(i16::MIN) saturates
    }

    #[test]
    fn transitions_active_to_quiescent_and_back() {
        let mut det = SilenceDetector::new(5, true);
        assert_eq!(det.state(), SilenceState::Active);

        // 4 silent packets, still not quiescent.
        for _ in 0..4 {
            let mut p = silent_packet(441);
            assert_eq!(det.process(&mut p), SilenceState::Active);
        }

        // 5th silent packet flips to quiescent.
        let mut p = silent_packet(441);
        assert_eq!(det.process(&mut p), SilenceState::Quiescent);

        // While quiescent, samples must have been rewritten with noise
        // (i.e. not all-zero, but very small).
        assert!(p.samples.iter().any(|s| *s != 0));
        let peak = peak_abs_i16(&p.samples);
        assert!(peak <= 4, "noise peak too high: {}", peak);

        // A loud packet flips us straight back.
        let mut loud = loud_packet(441);
        assert_eq!(det.process(&mut loud), SilenceState::Active);
        // And loud samples are NOT rewritten.
        assert!(loud.samples.iter().all(|s| *s == 10_000));
    }

    #[test]
    fn no_injection_when_disabled() {
        let mut det = SilenceDetector::new(2, false);
        for _ in 0..3 {
            let mut p = silent_packet(100);
            det.process(&mut p);
            assert!(p.samples.iter().all(|s| *s == 0));
        }
        assert_eq!(det.state(), SilenceState::Quiescent);
    }
}
