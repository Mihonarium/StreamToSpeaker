//! 440 Hz sine generator for testing the pipeline end-to-end with no input.

use anyhow::Result;
use std::time::{Duration, Instant};

use crate::audio_source::{AudioPacket, AudioSource};
use crate::{WIRE_CHANNELS, WIRE_SAMPLE_RATE};

/// Produces 10 ms packets of a 440 Hz sine wave at roughly -12 dBFS.
pub struct SineSource {
    phase: f32,
    /// Frames per packet (44_100 * 0.010 = 441).
    frames_per_packet: usize,
    /// When to emit the next packet (paces ourselves so we don't busy-loop).
    next_emit: Instant,
    packet_period: Duration,
    /// Cumulative frame count since start.
    stream_position: u64,
    /// Reusable scratch buffer.
    scratch: Vec<i16>,
}

impl SineSource {
    pub fn new() -> Self {
        let frames_per_packet = (WIRE_SAMPLE_RATE / 100) as usize; // 10 ms
        let scratch = vec![0i16; frames_per_packet * WIRE_CHANNELS as usize];
        Self {
            phase: 0.0,
            frames_per_packet,
            next_emit: Instant::now(),
            packet_period: Duration::from_millis(10),
            stream_position: 0,
            scratch,
        }
    }
}

impl Default for SineSource {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSource for SineSource {
    fn name(&self) -> &str {
        "sine-440"
    }

    fn recv_packet(&mut self) -> Result<AudioPacket> {
        // Pace ourselves.
        let now = Instant::now();
        if now < self.next_emit {
            std::thread::sleep(self.next_emit - now);
        }
        self.next_emit += self.packet_period;
        // Avoid drift when we fall behind: if we're way late, just resync.
        let now2 = Instant::now();
        if self.next_emit < now2 {
            self.next_emit = now2 + self.packet_period;
        }

        // -12 dBFS ~= 0.25 * i16::MAX.
        let amplitude: f32 = (i16::MAX as f32) * 0.25;
        let two_pi_f_over_fs: f32 =
            std::f32::consts::TAU * 440.0 / (WIRE_SAMPLE_RATE as f32);

        let chans = WIRE_CHANNELS as usize;
        for f in 0..self.frames_per_packet {
            let s = (self.phase.sin() * amplitude) as i16;
            for c in 0..chans {
                self.scratch[f * chans + c] = s;
            }
            self.phase += two_pi_f_over_fs;
            // Wrap to avoid precision loss on long runs.
            if self.phase > std::f32::consts::TAU {
                self.phase -= std::f32::consts::TAU;
            }
        }

        self.stream_position += self.frames_per_packet as u64;

        Ok(AudioPacket {
            samples: self.scratch.clone(),
            sample_rate: WIRE_SAMPLE_RATE,
            channels: WIRE_CHANNELS,
            timestamp_qpc: crate::qpc::query_performance_counter(),
            stream_position: self.stream_position,
            flags: 0,
        })
    }
}
