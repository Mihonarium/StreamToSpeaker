//! Streaming rational-ratio resampler (polyphase windowed sinc), used when
//! a Sendspin player cannot take our native 44.1 kHz (most commonly it
//! wants 48 kHz).
//!
//! For `out_rate / in_rate = L / M` (reduced), output sample `n` sits at
//! input position `n·M/L`; its value is the dot product of the input
//! history with the filter phase `(n·M) mod L`. The prototype low-pass is
//! a Kaiser-windowed sinc cut at 0.45 · min(in, out) — flat to ~20 kHz at
//! 44.1→48 kHz with ≥ 80 dB stop-band.

#[derive(Clone)]
pub struct Resampler {
    l: usize,
    m: usize,
    taps: usize,
    channels: usize,
    /// `phases[p][k]`, k = 0 is the newest input sample.
    phases: Vec<Vec<f32>>,
    /// Interleaved input history (newest last), `taps` frames.
    history: Vec<f32>,
    /// Position of the next output, in units of 1/L input frames,
    /// relative to the newest input frame consumed so far.
    pos: usize,
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Zeroth-order modified Bessel function (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..50 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        let g = gcd(in_rate as usize, out_rate as usize);
        let l = out_rate as usize / g;
        let m = in_rate as usize / g;
        // Taps per phase: more for a downsample (narrower cut-off).
        let taps = if out_rate >= in_rate { 32 } else { 48 };
        let total = l * taps;
        let cutoff = 0.45 * (out_rate.min(in_rate) as f64) / (in_rate as f64); // cycles/input sample
        let beta = 8.6;
        let i0b = bessel_i0(beta);
        let center = (total as f64 - 1.0) / 2.0;
        let mut proto = vec![0f64; total];
        for (i, c) in proto.iter_mut().enumerate() {
            // Time in input samples.
            let t = (i as f64 - center) / l as f64;
            let x = 2.0 * cutoff * t;
            let sinc = if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
            let r = (i as f64 - center) / center;
            let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
            *c = 2.0 * cutoff * sinc * w;
        }
        // Each phase sums to ~1/L of the total gain; scale so DC gain = 1.
        let mut phases = vec![vec![0f32; taps]; l];
        for p in 0..l {
            for k in 0..taps {
                // Coefficient for input sample k frames back from the
                // newest, at fractional offset p/L.
                let idx = p + k * l;
                phases[p][k] = (proto[idx] * l as f64) as f32;
            }
            let s: f32 = phases[p].iter().sum();
            if s.abs() > 1e-6 {
                for c in phases[p].iter_mut() {
                    *c /= s;
                }
            }
        }
        Self {
            l,
            m,
            taps,
            channels,
            phases,
            history: vec![0.0; taps * channels],
            pos: 0,
        }
    }

    pub fn is_identity(&self) -> bool {
        self.l == self.m
    }

    /// Group delay in output frames (constant).
    pub fn delay_frames(&self) -> usize {
        self.taps / 2 * self.l / self.m
    }

    /// Feed interleaved i16 frames; appends interleaved output.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        let ch = self.channels;
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        for frame in input.chunks_exact(ch) {
            // Push one input frame.
            self.history.drain(..ch);
            self.history.extend(frame.iter().map(|&s| s as f32));
            // Emit every output whose position falls before the next input.
            while self.pos < self.l {
                let phase = &self.phases[self.pos];
                for c in 0..ch {
                    let mut acc = 0f32;
                    for k in 0..self.taps {
                        acc += phase[k] * self.history[(self.taps - 1 - k) * ch + c];
                    }
                    out.push(acc.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
                }
                self.pos += self.m;
            }
            self.pos -= self.l;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, frames: usize, amp: f64) -> Vec<i16> {
        (0..frames)
            .flat_map(|i| {
                let s = (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin()) as i16;
                [s, s]
            })
            .collect()
    }

    fn analyse(samples: &[i16], rate: u32) -> (f64, f64) {
        let left: Vec<i16> = samples.chunks_exact(2).map(|c| c[0]).collect();
        let skip = 2000;
        let body = &left[skip..left.len() - skip];
        let zc = body.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        let freq = zc as f64 * rate as f64 / body.len() as f64;
        let peak = body.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f64;
        (freq, peak)
    }

    #[test]
    fn upsample_44k1_to_48k_preserves_tone_and_rate() {
        let mut r = Resampler::new(44_100, 48_000, 2);
        let input = sine(1000.0, 44_100, 44_100, 10_000.0);
        let mut out = Vec::new();
        // Feed in odd-sized pieces to exercise streaming state.
        for piece in input.chunks(2 * 441) {
            r.process(piece, &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "frames={}", frames);
        let (freq, peak) = analyse(&out, 48_000);
        assert!((freq - 1000.0).abs() < 3.0, "freq={}", freq);
        assert!((peak - 10_000.0).abs() < 150.0, "peak={}", peak);
    }

    /// Least-squares fit of a known-frequency sine to the output; the
    /// residual must be tiny (a wrong polyphase phase order shows up here
    /// as jitter noise long before it moves the zero-crossing count).
    #[test]
    fn upsampled_sine_is_clean() {
        let mut r = Resampler::new(44_100, 48_000, 2);
        let input = sine(3_000.0, 44_100, 22_050, 12_000.0);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let left: Vec<f64> = out.chunks_exact(2).map(|c| c[0] as f64).collect();
        let body = &left[1000..left.len() - 1000];
        let w = 2.0 * std::f64::consts::PI * 3_000.0 / 48_000.0;
        let (mut ss, mut sc, mut cc, mut ys, mut yc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, y) in body.iter().enumerate() {
            let (s, c) = ((w * i as f64).sin(), (w * i as f64).cos());
            ss += s * s;
            sc += s * c;
            cc += c * c;
            ys += y * s;
            yc += y * c;
        }
        let det = ss * cc - sc * sc;
        let a = (ys * cc - yc * sc) / det;
        let b = (yc * ss - ys * sc) / det;
        let mut err = 0.0;
        let mut pow = 0.0;
        for (i, y) in body.iter().enumerate() {
            let fit = a * (w * i as f64).sin() + b * (w * i as f64).cos();
            err += (y - fit) * (y - fit);
            pow += fit * fit;
        }
        let snr_db = 10.0 * (pow / err).log10();
        assert!(snr_db > 60.0, "SNR {:.1} dB", snr_db);
    }

    #[test]
    fn high_frequencies_above_new_nyquist_are_attenuated_on_downsample() {
        let mut r = Resampler::new(48_000, 44_100, 2);
        // 23 kHz is above 44.1 kHz's Nyquist: must not alias in loudly.
        let input = sine(23_000.0, 48_000, 48_000, 10_000.0);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let (_, peak) = analyse(&out, 44_100);
        assert!(peak < 200.0, "aliased peak {}", peak);
    }

    #[test]
    fn identity_passes_through() {
        let mut r = Resampler::new(44_100, 44_100, 2);
        assert!(r.is_identity());
        let mut out = Vec::new();
        r.process(&[1, 2, 3, 4], &mut out);
        assert_eq!(out, vec![1, 2, 3, 4]);
    }
}
