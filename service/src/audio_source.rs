//! Audio source abstraction.
//!
//! All sources produce interleaved i16 stereo PCM at 44.1 kHz. Sources that
//! deliver a different native format MUST convert before returning, so the
//! rest of the pipeline only sees the wire format.

use anyhow::Result;

/// One packet of PCM samples emitted by an audio source.
///
/// The `samples` buffer is interleaved `[L0, R0, L1, R1, ...]`. The vector
/// may be reused across calls — implementations are encouraged to keep a
/// scratch `Vec<i16>` and refill it in place to avoid allocations on the
/// realtime audio thread.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
    pub timestamp_qpc: u64,
    /// Cumulative frame count since the source started.
    pub stream_position: u64,
    /// Driver hints / flags from the source. Bit 0 = stream restart.
    pub flags: u32,
}

/// Bit set when a source signals a new stream (format change, re-open).
pub const PACKET_FLAG_STREAM_RESTART: u32 = 0x0000_0001;
/// Bit set when the source hints that the packet is entirely silent.
pub const PACKET_FLAG_HINT_SILENT: u32 = 0x0000_0002;

impl AudioPacket {
    /// Construct an empty packet. Useful as a sentinel; not normally
    /// produced by real sources.
    pub fn empty() -> Self {
        Self {
            samples: Vec::new(),
            sample_rate: crate::WIRE_SAMPLE_RATE,
            channels: crate::WIRE_CHANNELS,
            timestamp_qpc: 0,
            stream_position: 0,
            flags: 0,
        }
    }

    /// Construct a silent packet of `n_frames` stereo frames at the wire
    /// format. Used by the main loop and by the IOCTL source when the
    /// driver signals a stream stop (it drains pending IRPs with zero
    /// bytes — see `IoctlOnStreamStop`). The PACKET_FLAG_HINT_SILENT bit
    /// lets the silence detector skip its scan.
    pub fn silence(n_frames: usize) -> Self {
        let channels = crate::WIRE_CHANNELS;
        Self {
            samples: vec![0i16; n_frames * channels as usize],
            sample_rate: crate::WIRE_SAMPLE_RATE,
            channels,
            timestamp_qpc: 0,
            stream_position: 0,
            flags: PACKET_FLAG_HINT_SILENT,
        }
    }

    /// Number of sample frames (L+R pairs) in this packet.
    pub fn frame_count(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }
}

/// Source of PCM packets. `recv_packet` blocks until a packet is available
/// or returns an error (e.g. driver handle closed, shutdown requested).
pub trait AudioSource: Send {
    fn recv_packet(&mut self) -> Result<AudioPacket>;

    /// Optional human-readable name for logs.
    fn name(&self) -> &str {
        "audio-source"
    }
}
