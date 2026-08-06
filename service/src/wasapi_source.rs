//! WASAPI-loopback fallback source — Windows only.
#![cfg(windows)]

//! WASAPI-loopback fallback source, using `cpal`.
//!
//! When the kernel driver isn't installed we capture the system mix from
//! the default render endpoint and stream that to Sonos. This mirrors what
//! swyh-rs does and is good enough for casual use.
//!
//! cpal 0.15+ on Windows treats the default *output* device as openable for
//! input via WASAPI loopback. We pull samples (which may be f32 / i16 /
//! u16, depending on the device's shared-mode mix format), convert to
//! interleaved i16 stereo at 44.1 kHz, and ship them downstream.

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, StreamConfig};
use crossbeam_channel::{bounded, Receiver, Sender};
use log::{debug, info, warn};

use crate::audio_source::{AudioPacket, AudioSource};
use crate::{WIRE_CHANNELS, WIRE_SAMPLE_RATE};

/// Frames per emitted packet downstream (10 ms @ 44.1 kHz).
const FRAMES_PER_PACKET: usize = 441;

/// Captured chunk straight from cpal: already i16 stereo @ native rate.
struct CapturedChunk {
    samples: Vec<i16>,
    src_rate: u32,
    src_channels: u16,
}

/// Loopback source built on top of cpal.
pub struct WasapiLoopbackSource {
    // The cpal::Stream is !Send, so we keep it on a dedicated thread; this
    // handle lets us tell that thread to drop the stream and exit.
    _worker: WorkerJoin,
    rx: Receiver<CapturedChunk>,

    // Resampler state (linear interpolation in time domain).
    /// Last sample per channel from the previous chunk (for cross-chunk
    /// interpolation continuity).
    last_l: f32,
    last_r: f32,
    /// Fractional accumulator for the output clock relative to the input
    /// clock (range [0, 1)).
    frac: f32,

    /// Pending interleaved i16 stereo @ 44.1 kHz, waiting to be packetised.
    pending: Vec<i16>,
    /// Reusable output buffer.
    scratch: Vec<i16>,
    /// Cumulative output frame counter.
    stream_position: u64,
    /// Native source format for log lines.
    src_rate: u32,
    src_channels: u16,
}

impl WasapiLoopbackSource {
    /// Build a loopback source.  `device_name` matches a substring against
    /// the device's reported name; pass `None` for the default output
    /// device.
    pub fn new(device_name: Option<&str>) -> Result<Self> {
        let (tx_chunk, rx_chunk) = bounded::<CapturedChunk>(64);
        let (tx_ready, rx_ready) = bounded::<Result<(u32, u16)>>(1);
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let device_name_owned = device_name.map(|s| s.to_string());
        let shutdown_for_thread = shutdown.clone();

        // cpal::Stream is !Send. Build, hold, and drop it all on one
        // dedicated thread.
        let join = std::thread::Builder::new()
            .name("stream-to-speaker-wasapi-loopback".to_string())
            .spawn(move || {
                let res = build_loopback_stream(device_name_owned.as_deref(), tx_chunk.clone());
                match res {
                    Ok((stream, src_rate, src_channels)) => {
                        if tx_ready.send(Ok((src_rate, src_channels))).is_err() {
                            return;
                        }
                        // Hold the stream alive until shutdown.
                        while !shutdown_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = tx_ready.send(Err(e));
                    }
                }
            })
            .context("spawning wasapi loopback worker")?;

        let (src_rate, src_channels) = rx_ready
            .recv()
            .map_err(|_| anyhow!("wasapi worker died before ready"))??;

        Ok(Self {
            _worker: WorkerJoin {
                shutdown,
                handle: Some(join),
            },
            rx: rx_chunk,
            last_l: 0.0,
            last_r: 0.0,
            frac: 0.0,
            pending: Vec::with_capacity(FRAMES_PER_PACKET * 4),
            scratch: vec![0i16; FRAMES_PER_PACKET * WIRE_CHANNELS as usize],
            stream_position: 0,
            src_rate,
            src_channels,
        })
    }

    /// Pull at least one chunk from cpal, convert to 44.1 kHz stereo i16,
    /// and push the result into `self.pending`.
    fn refill(&mut self) -> Result<()> {
        // Block on at least one chunk.
        let chunk = self
            .rx
            .recv()
            .map_err(|_| anyhow!("wasapi capture stream closed"))?;
        self.absorb_chunk(chunk);

        // Drain anything else that's already queued, opportunistically.
        while let Ok(chunk) = self.rx.try_recv() {
            self.absorb_chunk(chunk);
        }
        Ok(())
    }

    fn absorb_chunk(&mut self, chunk: CapturedChunk) {
        if chunk.src_channels == 0 {
            return;
        }
        let src_channels = chunk.src_channels as usize;
        let src_rate = chunk.src_rate.max(1);

        // Step 1: downmix to stereo (or up-mix if mono).
        let frames = chunk.samples.len() / src_channels;
        let mut stereo: Vec<(f32, f32)> = Vec::with_capacity(frames);
        for f in 0..frames {
            let base = f * src_channels;
            let (l, r) = match src_channels {
                1 => {
                    let s = chunk.samples[base] as f32;
                    (s, s)
                }
                _ => {
                    let l = chunk.samples[base] as f32;
                    let r = chunk.samples[base + 1] as f32;
                    (l, r)
                }
            };
            stereo.push((l, r));
        }

        if stereo.is_empty() {
            return;
        }

        // Step 2: linear-interpolation resample from src_rate -> 44_100.
        let ratio = (src_rate as f32) / (WIRE_SAMPLE_RATE as f32);
        // Output sample index t corresponds to input sample index t * ratio.
        // We iterate until we'd need to look past the end of `stereo`.
        let mut frac = self.frac;
        let mut prev_l = self.last_l;
        let mut prev_r = self.last_r;

        // For continuity across chunks, virtual sample index -1 in the
        // current chunk is `prev_l/prev_r`.  We treat input as
        // [(prev_l,prev_r), stereo[0], stereo[1], ... stereo[n-1]] indexed
        // 0..=n, with idx=0 -> prev sample.
        let n_in = stereo.len();

        loop {
            // Position into the *combined* sequence (prev + stereo).
            // out_pos in input-units = frac, starts at frac (cumulative).
            let in_idx_f = frac;
            let in_idx = in_idx_f.floor() as i32;
            let alpha = in_idx_f - in_idx as f32;

            // We need input samples at in_idx and in_idx+1.
            // in_idx == 0 -> prev sample. in_idx >= 1 -> stereo[in_idx - 1].
            // We can produce an output sample as long as in_idx+1 <= n_in.
            if in_idx + 1 > n_in as i32 {
                // Need more input.
                break;
            }

            let (a_l, a_r) = sample_at(in_idx, prev_l, prev_r, &stereo);
            let (b_l, b_r) = sample_at(in_idx + 1, prev_l, prev_r, &stereo);

            let l = a_l + (b_l - a_l) * alpha;
            let r = a_r + (b_r - a_r) * alpha;

            self.pending.push(clamp_to_i16(l));
            self.pending.push(clamp_to_i16(r));

            frac += ratio;
        }

        // Carry frac into [0,1) and remember the latest input sample as
        // "prev" for the next chunk.
        // Move frac down by the integer part we have consumed; everything
        // up to `n_in` has been processed.
        // Concretely: the next chunk's input index 0 is `stereo[n_in-1]`'s
        // successor.  So we shift frac by n_in.
        frac -= n_in as f32;
        // Numerical guard.
        if !frac.is_finite() {
            frac = 0.0;
        }
        // Keep frac in something sensible.
        while frac < 0.0 {
            frac += 1.0;
        }

        self.frac = frac;
        if let Some(&(l, r)) = stereo.last() {
            prev_l = l;
            prev_r = r;
        }
        self.last_l = prev_l;
        self.last_r = prev_r;
        let _ = (self.src_rate, self.src_channels); // suppress unused on some cfgs
    }
}

#[inline]
fn sample_at(in_idx: i32, prev_l: f32, prev_r: f32, stereo: &[(f32, f32)]) -> (f32, f32) {
    if in_idx <= 0 {
        (prev_l, prev_r)
    } else {
        let i = (in_idx - 1) as usize;
        if i < stereo.len() {
            stereo[i]
        } else {
            // Caller guarantees this doesn't happen, but be safe.
            *stereo.last().unwrap_or(&(0.0, 0.0))
        }
    }
}

#[inline]
fn clamp_to_i16(v: f32) -> i16 {
    if v >= i16::MAX as f32 {
        i16::MAX
    } else if v <= i16::MIN as f32 {
        i16::MIN
    } else {
        v as i16
    }
}

#[inline]
fn f32_to_i16(v: f32) -> i16 {
    let scaled = v * (i16::MAX as f32);
    clamp_to_i16(scaled)
}

impl AudioSource for WasapiLoopbackSource {
    fn name(&self) -> &str {
        "wasapi-loopback"
    }

    fn recv_packet(&mut self) -> Result<AudioPacket> {
        let target = FRAMES_PER_PACKET * WIRE_CHANNELS as usize;
        while self.pending.len() < target {
            self.refill()?;
        }

        // Drain `target` samples into scratch.
        // (Copy out the front, then keep the tail.)
        self.scratch.clear();
        self.scratch.extend(self.pending.drain(..target));

        self.stream_position += FRAMES_PER_PACKET as u64;
        debug!(
            "wasapi-loopback emit: {} frames, pending={} samples",
            FRAMES_PER_PACKET,
            self.pending.len()
        );

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
/// Worker join handle wrapper that signals shutdown on drop.
struct WorkerJoin {
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WorkerJoin {
    fn drop(&mut self) {
        self.shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Build a cpal loopback stream on the *current* thread. Returns the
/// stream (which the caller must keep alive on the same thread) plus the
/// stream's source sample rate / channels.
fn build_loopback_stream(
    device_name: Option<&str>,
    tx: Sender<CapturedChunk>,
) -> Result<(cpal::Stream, u32, u16)> {
    let host = cpal::default_host();

    let device = if let Some(name) = device_name {
        let lower = name.to_ascii_lowercase();
        host.output_devices()
            .context("enumerating output devices")?
            .find(|d| {
                d.name()
                    .map(|n| n.to_ascii_lowercase().contains(&lower))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow!("no output device matches {:?}", name))?
    } else {
        host.default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?
    };

    let dev_name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
    let config = device
        .default_output_config()
        .with_context(|| format!("default_output_config on {}", dev_name))?;

    let sample_format = config.sample_format();
    let stream_config: StreamConfig = config.into();
    let src_rate = stream_config.sample_rate.0;
    let src_channels = stream_config.channels;

    info!(
        "wasapi-loopback: device={} format={:?} rate={} channels={}",
        dev_name, sample_format, src_rate, src_channels
    );

    let err_fn = |e| warn!("wasapi-loopback stream error: {}", e);

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut out = Vec::with_capacity(data.len());
                for &v in data {
                    out.push(f32_to_i16(v));
                }
                let _ = tx.try_send(CapturedChunk {
                    samples: out,
                    src_rate,
                    src_channels,
                });
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let out = data.to_vec();
                let _ = tx.try_send(CapturedChunk {
                    samples: out,
                    src_rate,
                    src_channels,
                });
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _: &cpal::InputCallbackInfo| {
                let mut out = Vec::with_capacity(data.len());
                for &v in data {
                    let s: i16 = i16::from_sample(v);
                    out.push(s);
                }
                let _ = tx.try_send(CapturedChunk {
                    samples: out,
                    src_rate,
                    src_channels,
                });
            },
            err_fn,
            None,
        ),
        other => bail!("unsupported cpal sample format: {:?}", other),
    }
    .with_context(|| format!("building loopback stream on {}", dev_name))?;

    stream.play().context("starting wasapi loopback stream")?;
    Ok((stream, src_rate, src_channels))
}
