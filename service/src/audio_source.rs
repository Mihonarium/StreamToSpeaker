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

/// Which PC outputs can play captured samples without feeding them back into capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalPlayback {
    Any,
    Except(crate::model::PcOutputId),
    /// Capture follows Windows’ default because the app’s output is absent.
    WindowsDefault,
    /// The output opened by name could not be identified for local playback.
    None,
}

impl LocalPlayback {
    pub(crate) fn allows(&self, output: &crate::model::PcOutputId) -> bool {
        match self { Self::Any => true, Self::Except(id) => id != output, Self::WindowsDefault | Self::None => false }
    }

    #[cfg(any(windows, test))]
    pub(crate) fn for_named_output(outputs: &dyn crate::platform::Outputs, name: &str) -> Self {
        let ours = outputs.our_output();
        let listed = outputs.list();
        let mut matches = listed.iter().filter(|o| {
            if Some(&o.id) == ours.as_ref() {
                outputs.current_friendly(&o.id).as_deref() == Some(name)
            } else { o.friendly == name }
        });
        match (matches.next(), matches.next()) {
            (Some(o), None) => Self::Except(o.id.clone()),
            _ => Self::None,
        }
    }
}

/// Source of PCM packets. `recv_packet` blocks until a packet is available
/// or returns an error (e.g. driver handle closed, shutdown requested).
pub trait AudioSource: Send {
    fn recv_packet(&mut self) -> Result<AudioPacket>;

    /// The endpoint this source follows changed while it was open.
    fn needs_reopen(&self) -> bool { false }

    fn local_playback(&self) -> LocalPlayback { LocalPlayback::Any }

    /// Optional human-readable name for logs.
    fn name(&self) -> &str {
        "audio-source"
    }
}

#[cfg(any(windows, test))]
pub(crate) struct LoopbackEndpoint {
    outputs: std::sync::Arc<dyn crate::platform::Outputs>,
    ours: Option<crate::model::state::PcOutputId>,
    default: Option<crate::model::state::PcOutputId>,
}

#[cfg(any(windows, test))]
impl LoopbackEndpoint {
    pub(crate) fn new(outputs: std::sync::Arc<dyn crate::platform::Outputs>) -> Self {
        let ours = outputs.our_output();
        let default = if ours.is_none() { outputs.default_output() } else { None };
        Self { outputs, ours, default }
    }

    /// The exact name to open. With no app output, open Windows' default.
    pub(crate) fn name(&self) -> Result<Option<String>> {
        self.ours.as_ref().map(|id| self.outputs.current_friendly(id)
            .ok_or_else(|| anyhow::anyhow!("Stream To Speaker’s sound output is unavailable"))).transpose()
    }

    pub(crate) fn local_playback(&self) -> LocalPlayback {
        if self.ours.is_some() { LocalPlayback::Any } else { LocalPlayback::WindowsDefault }
    }

    pub(crate) fn changed(&self) -> bool {
        self.outputs.our_output() != self.ours
            || (self.ours.is_none() && self.outputs.default_output() != self.default)
    }
}
