//! Audio loop: pulls packets from the AudioSource, processes them, and
//! publishes to the StreamHub. Designed to run on a dedicated thread so
//! it doesn't block the GUI event loop.

use anyhow::Result;
use log::{debug, error, info, warn};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::audio_source::{AudioPacket, AudioSource, PACKET_FLAG_HINT_SILENT, PACKET_FLAG_STREAM_RESTART};
use crate::http_server::{samples_to_l16_be_bytes, PcmFrame};
use crate::silence::SilenceDetector;
use crate::{WIRE_CHANNELS, WIRE_SAMPLE_RATE};

/// Run the audio loop until `app.shutdown` is set. Takes ownership of
/// the source so the source is dropped (and any kernel handles released)
/// when this returns.
pub fn run(app: Arc<App>, mut source: Box<dyn AudioSource>) -> Result<()> {
    info!("audio loop: source = {}", source.name());

    let mut silence = SilenceDetector::new(
        app.config.silence_packets_threshold,
        !app.config.no_silence_injection,
    );

    // 10 ms silence packets when the driver is idle (stream not in
    // KSSTATE_RUN). Pacing controlled by silence_pace_ms atomic.
    const SILENCE_FRAMES: usize = WIRE_SAMPLE_RATE as usize / 100;
    let mut next_silence_deadline = Instant::now();

    // Rate-fudge accumulator for clock-skew compensation.
    let mut fudge_accum: f64 = 0.0;

    let mut last_packet_log = Instant::now();
    let mut packet_count_since_log: u64 = 0;

    loop {
        if app.is_shutting_down() {
            break;
        }

        let active = app.stream_active.load(Ordering::Acquire);
        let mut pkt = if active {
            match source.recv_packet() {
                Ok(p) => p,
                Err(e) => {
                    if app.is_shutting_down() {
                        break;
                    }
                    error!("audio source error: {} — exiting loop", e);
                    return Err(e);
                }
            }
        } else {
            // Idle: emit paced silence so the speaker doesn't time out
            // the HTTP stream. Uses a deadline so a previous overrun is
            // absorbed instead of compounding into latency.
            let pace = Duration::from_millis(
                app.silence_pace_ms.load(Ordering::Relaxed).max(1),
            );
            let now = Instant::now();
            if next_silence_deadline > now {
                std::thread::sleep(next_silence_deadline - now);
            }
            next_silence_deadline = next_silence_deadline.max(now) + pace;
            AudioPacket::silence(SILENCE_FRAMES)
        };

        if pkt.flags & PACKET_FLAG_STREAM_RESTART != 0 {
            info!("stream restart from source (new session)");
        }

        if pkt.sample_rate != WIRE_SAMPLE_RATE || pkt.channels != WIRE_CHANNELS {
            warn!(
                "unexpected source format: rate={} ch={}; expected {}/{}",
                pkt.sample_rate, pkt.channels, WIRE_SAMPLE_RATE, WIRE_CHANNELS,
            );
            continue;
        }

        // Driver signals "stream stop" by completing the IOCTL with a
        // zero-filled 10 ms packet. Flip the active flag so the next
        // iteration emits paced silence directly, no blocking IOCTL.
        if active
            && !pkt.samples.is_empty()
            && pkt.flags & PACKET_FLAG_HINT_SILENT != 0
            && pkt.samples.iter().all(|&s| s == 0)
        {
            app.stream_active.store(false, Ordering::Release);
            next_silence_deadline = Instant::now()
                + Duration::from_millis(app.silence_pace_ms.load(Ordering::Relaxed).max(1));
            debug!("audio: stream-stop drain received; emitting silence until StreamStart");
        }

        silence.process(&mut pkt);

        // Rate-fudge: insert / drop a frame when the accumulator crosses ±1.0.
        let fudge_ppm = app.rate_fudge_ppm.load(Ordering::Relaxed);
        if fudge_ppm != 0 && !pkt.samples.is_empty() {
            let frames_in_pkt = pkt.samples.len() / pkt.channels as usize;
            fudge_accum += (frames_in_pkt as f64) * (fudge_ppm as f64) * 1e-6;
            let ch = pkt.channels as usize;
            if fudge_accum >= 1.0 {
                fudge_accum -= 1.0;
                let n = pkt.samples.len();
                if n >= ch {
                    let mut last = Vec::with_capacity(ch);
                    last.extend_from_slice(&pkt.samples[n - ch..]);
                    pkt.samples.extend_from_slice(&last);
                }
            } else if fudge_accum <= -1.0 {
                fudge_accum += 1.0;
                let n = pkt.samples.len();
                if n >= ch {
                    pkt.samples.truncate(n - ch);
                }
            }
        }

        // Runtime latency-adjust: drop / duplicate up to N frames per packet.
        if !pkt.samples.is_empty() {
            let pending = app.drain_frames.load(Ordering::Acquire);
            if pending != 0 {
                let ch = pkt.channels as usize;
                let max_step =
                    app.latency_adjust_step_frames.load(Ordering::Relaxed).max(1) as i64;
                let step = pending.signum() * pending.abs().min(max_step);
                if step > 0 {
                    let drop_samples = (step as usize) * ch;
                    let new_len = pkt.samples.len().saturating_sub(drop_samples);
                    pkt.samples.truncate(new_len);
                    app.drain_frames.fetch_sub(step, Ordering::AcqRel);
                } else if step < 0 && pkt.samples.len() >= ch {
                    let start = pkt.samples.len() - ch;
                    let last_frame: Vec<i16> = pkt.samples[start..].to_vec();
                    for _ in 0..(-step) {
                        pkt.samples.extend_from_slice(&last_frame);
                    }
                    app.drain_frames.fetch_sub(step, Ordering::AcqRel);
                }
            }
        }

        let bytes = samples_to_l16_be_bytes(&pkt.samples);
        app.hub.publish(PcmFrame(Arc::new(bytes)));
        app.packets_published_total.fetch_add(1, Ordering::Relaxed);

        packet_count_since_log += 1;
        if last_packet_log.elapsed() >= Duration::from_secs(10) {
            debug!(
                "{} packets streamed in last 10s, {} subscriber(s), pending latency-adjust {} frames",
                packet_count_since_log,
                app.hub.subscriber_count(),
                app.drain_frames.load(Ordering::Relaxed),
            );
            packet_count_since_log = 0;
            last_packet_log = Instant::now();
        }
    }

    info!("audio loop: exiting");
    Ok(())
}
