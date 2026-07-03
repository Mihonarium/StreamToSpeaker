//! RTP audio packetizer + sender thread.
//!
//! Per the RAOP convention every audio packet carries exactly 352
//! stereo sample frames (the `frame_length` advertised in the ANNOUNCE
//! SDP). The driver source delivers 10 ms packets (= 441 frames at 44.1
//! kHz), so we re-frame: accumulate samples in a ring, slice into 352-
//! frame chunks, build an ALAC frame, encrypt, and send.
//!
//! Audio scheduling is paced to wall-clock: each packet is 352 / 44100
//! ≈ 7.98 ms of audio, so we sleep until the cumulative deadline before
//! sending. **If the upstream source stalls, we synthesize silence** so
//! the rtptime stays glued to wall-clock: the 1 Hz sync packets anchor
//! rtptime→NTP on the receiver, and audio arriving with an rtptime that
//! lags the advancing anchor is classified "late" and silently discarded
//! (field-proven on Sonos in AP2 buffered mode; same rule here). The
//! driver only starts delivering frames at the first KSSTATE_RUN, so a
//! session opened before any app plays audio WILL stall without this.
//!
//! Audio is encoded as **real compressed ALAC** (Apple's encoder, the
//! `alac-encoder` port) — what iTunes, OwnTone, AirConnect and
//! node_airtunes2 all send. The uncompressed-ALAC escape we used to
//! ship is only proven on shairport-class receivers; the one field
//! sender that uses it (PipeWire) reproduces the connected-but-silent
//! symptom on Sonos. `uncompressed_alac` in the config flips back for
//! A/B debugging.
//!
//! ## RTP packet layout
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |V=2|P|X| CC=0  |M|     PT=96     |       sequence number         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                           timestamp                           |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                             SSRC                              |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                  encrypted ALAC frame ...
//! ```
//!
//! Marker bit (M) is set on the very first audio packet of a session;
//! 0 thereafter. We deliberately do NOT set it on the per-resync first
//! packet — RAOP receivers treat M=1 as a "this is the start of a
//! brand-new stream" flush, which would re-introduce the prebuffer
//! latency.

use alac_encoder::{AlacEncoder, FormatDescription};
use anyhow::{Context, Result};
use byteorder::{BigEndian, ByteOrder};
use crossbeam_channel::{Receiver, TryRecvError};
use log::{debug, info, warn};
use rand::Rng;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::airplay::alac::build_uncompressed_alac_frame;
use crate::airplay::crypto::Cipher;
use crate::airplay::timing::ResendBuffer;
use crate::http_server::PcmFrame;
use crate::WIRE_SAMPLE_RATE;

/// Sample frames per RTP packet — matches the ANNOUNCE `fmtp` value.
pub const FRAMES_PER_PACKET: usize = 352;

/// Bytes of PCM per packet: 352 frames × 2 channels × 2 bytes.
const PACKET_BYTES: usize = FRAMES_PER_PACKET * 4;

/// One packet of digital silence, used by the silence-fill path.
const SILENCE_PCM: [u8; PACKET_BYTES] = [0u8; PACKET_BYTES];

/// If the sender falls further behind wall-clock than this, it re-glues:
/// the missed packet slots are skipped on the RTP timeline instead of
/// replayed (libraop re-glues its head timestamp the same way on
/// restart). Replaying a long gap — e.g. after system suspend/resume —
/// would blast thousands of unpaced packets that the receiver discards
/// as late anyway.
const MAX_BEHIND: Duration = Duration::from_secs(1);

/// Cap on buffered-but-unsent audio. A transient underrun inserts one
/// silence packet ahead of the late-arriving real samples, adding ~8 ms
/// of permanent latency each time; without a cap those events accumulate
/// over a long session. Past the high-water mark the oldest samples are
/// dropped back down (one brief audible skip, bounded total latency).
const RING_HIGH_WATER_BYTES: usize = PACKET_BYTES * 4; // ~32 ms
const RING_DROP_TO_BYTES: usize = PACKET_BYTES * 2;

/// RTP payload type. RAOP fixes this at 96 (dynamic, first in range).
const RTP_PT_AUDIO: u8 = 0x60;

/// RTP version 2 in the top two bits of byte 0.
const RTP_V2: u8 = 0x80;

/// Initial RTP sequence number — we pick a random 16-bit value at
/// session start so successive sessions don't collide in any receiver-
/// side sequence-tracking state.
pub fn random_initial_seq() -> u16 {
    rand::thread_rng().gen()
}

/// Initial RTP timestamp — random 32-bit per RFC 3550 recommendation.
pub fn random_initial_rtptime() -> u32 {
    rand::thread_rng().gen()
}

/// Random SSRC for the session.
pub fn random_ssrc() -> u32 {
    rand::thread_rng().gen()
}

/// Configuration passed into the audio sender thread.
pub struct RtpSenderConfig {
    /// Local UDP socket to send audio from. Bound by the session
    /// before the thread starts so its local port can be reported in
    /// SETUP.
    pub audio_socket: UdpSocket,
    /// Receiver IP + port — destination for the audio stream (from
    /// the SETUP response's `server_port=`).
    pub receiver_addr: SocketAddr,
    /// Per-session cipher — either AES-RSA or no-op for receivers
    /// that advertise `et=0`. Shared with the packet builder by Arc
    /// so we don't clone the (potentially large) SessionKey buffer
    /// every packet.
    pub cipher: Arc<Cipher>,
    /// Initial RTP sequence number (echoed in the RECORD `RTP-Info`
    /// header, so must match what the session declared).
    pub initial_seq: u16,
    /// Initial RTP timestamp.
    pub initial_rtptime: u32,
    /// SSRC.
    pub ssrc: u32,
    /// PCM source — a subscription to `StreamHub`. PcmFrame.0 holds
    /// little-endian i16 samples, interleaved L,R,L,R,...
    pub samples_rx: Receiver<PcmFrame>,
    /// Stop signal — when true, the thread drains its pending audio
    /// and exits.
    pub stop_flag: Arc<AtomicBool>,
    /// Friendly receiver name, for log lines.
    pub receiver_name: String,
    /// Shared "current RTP timestamp" the sync packet sender reads off
    /// once per second so it can anchor its `RTP - latency` value to
    /// the same clock we're advancing here.
    pub current_rtptime: Arc<AtomicU32>,
    /// Recent-packet ring so the resend responder can retransmit packets
    /// the receiver reports missing.
    pub resend: Arc<ResendBuffer>,
    /// Debug escape hatch: ship the uncompressed-ALAC escape instead of
    /// real compressed ALAC (see module docs). Default false.
    pub uncompressed_alac: bool,
}

/// Spawn the audio sender thread. Returns once the thread is running.
/// The thread terminates when `stop_flag` is set OR when the PCM channel
/// disconnects (which happens when the StreamHub drops the subscription).
pub fn spawn_audio_sender(cfg: RtpSenderConfig) -> Result<std::thread::JoinHandle<()>> {
    let name = format!("stream-to-speaker-airplay-rtp:{}", cfg.receiver_name);
    let handle = std::thread::Builder::new()
        .name(name)
        .spawn(move || run_sender(cfg))
        .context("spawning AirPlay audio sender")?;
    Ok(handle)
}

fn run_sender(cfg: RtpSenderConfig) {
    info!(
        "AirPlay RTP sender starting → {} (initial seq={}, rtptime={}, codec={})",
        cfg.receiver_addr,
        cfg.initial_seq,
        cfg.initial_rtptime,
        if cfg.uncompressed_alac { "alac-escape" } else { "alac" },
    );

    let mut seq = cfg.initial_seq;
    let mut rtptime = cfg.initial_rtptime;
    let mut packet_count: u64 = 0;
    let mut silence_packets: u64 = 0;
    // PCM staging ring, raw LE bytes — the same layout PcmFrame delivers
    // and the ALAC encoder consumes, so the default path never converts.
    let mut sample_ring: Vec<u8> = Vec::with_capacity(PACKET_BYTES * 8);
    let mut pkt_pcm = [0u8; PACKET_BYTES];
    let mut disconnected = false;

    // Real compressed ALAC (Apple's encoder). Stateful per session —
    // retained predictor coefficients across frames improve the ratio.
    let output_format = FormatDescription::alac(WIRE_SAMPLE_RATE as f64, FRAMES_PER_PACKET as u32, 2);
    let input_format = FormatDescription::pcm::<i16>(WIRE_SAMPLE_RATE as f64, 2);
    let mut encoder = AlacEncoder::new(&output_format);
    let mut alac_buf = vec![0u8; output_format.max_packet_size()];

    // Drop any frames that queued up while the RTSP handshake ran —
    // they're seconds stale, and pacing means a backlog never drains,
    // so it would otherwise become permanent added latency.
    let mut stale = 0usize;
    loop {
        match cfg.samples_rx.try_recv() {
            Ok(_) => stale += 1,
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                info!("AirPlay RTP sender: PCM source disconnected, exiting");
                return;
            }
        }
    }
    if stale > 0 {
        debug!("AirPlay RTP sender: dropped {} stale pre-handshake frames", stale);
    }

    let start = Instant::now();
    let packet_duration = Duration::from_nanos(
        ((FRAMES_PER_PACKET as u64) * 1_000_000_000) / (WIRE_SAMPLE_RATE as u64),
    );

    loop {
        if cfg.stop_flag.load(Ordering::Acquire) {
            debug!("AirPlay RTP sender: stop flag set, exiting");
            break;
        }

        // One packet per iteration, sent at its absolute wall-clock
        // deadline (computed from packet_count so jitter never drifts).
        let deadline = start + packet_duration.saturating_mul((packet_count + 1) as u32);

        // Re-glue after a long stall (system suspend, debugger pause):
        // skip the missed slots on the RTP timeline instead of replaying
        // them, and drop the stale backlog from before the stall.
        let behind = Instant::now().saturating_duration_since(deadline);
        if behind > MAX_BEHIND {
            let missed = (behind.as_nanos() / packet_duration.as_nanos().max(1)) as u64 + 1;
            packet_count += missed;
            rtptime = rtptime.wrapping_add((missed as u32).wrapping_mul(FRAMES_PER_PACKET as u32));
            cfg.current_rtptime.store(rtptime, Ordering::Release);
            sample_ring.clear();
            while let Ok(_stale) = cfg.samples_rx.try_recv() {}
            warn!(
                "AirPlay RTP sender: stalled {:.1} s; skipped {} packet slots to stay \
                 glued to wall-clock",
                behind.as_secs_f32(),
                missed
            );
            continue;
        }

        // Fill the ring until we have a full packet or the deadline hits.
        // The opportunistic drain runs even when we're already past the
        // deadline, so queued real audio is preferred over silence-fill
        // when we're running marginally behind.
        loop {
            loop {
                match cfg.samples_rx.try_recv() {
                    Ok(frame) => append_frame_bytes(&mut sample_ring, &frame),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected || sample_ring.len() >= PACKET_BYTES {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = (deadline - now).min(Duration::from_millis(50));
            match cfg.samples_rx.recv_timeout(wait) {
                Ok(frame) => append_frame_bytes(&mut sample_ring, &frame),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if cfg.stop_flag.load(Ordering::Acquire) {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                }
            }
        }
        if cfg.stop_flag.load(Ordering::Acquire) {
            break;
        }
        if disconnected && sample_ring.len() < PACKET_BYTES {
            info!("AirPlay RTP sender: PCM source disconnected, exiting");
            break;
        }

        // Cap accumulated backlog: each transient underrun shifts queued
        // real audio one packet later; drop the oldest samples once the
        // creep exceeds the high-water mark so session latency stays
        // bounded (~32 ms) instead of growing for hours.
        if sample_ring.len() > RING_HIGH_WATER_BYTES {
            let drop_bytes = sample_ring.len() - RING_DROP_TO_BYTES;
            sample_ring.drain(..drop_bytes);
            debug!(
                "AirPlay RTP sender: dropped {} bytes of backlog to cap latency",
                drop_bytes
            );
        }

        // Real audio if a full packet is ready; otherwise silence-fill
        // so rtptime stays glued to wall-clock (any partial audio stays
        // queued for the next packet — chronological order preserved).
        let pcm: &[u8] = if sample_ring.len() >= PACKET_BYTES {
            pkt_pcm.copy_from_slice(&sample_ring[..PACKET_BYTES]);
            sample_ring.drain(..PACKET_BYTES);
            &pkt_pcm
        } else {
            silence_packets += 1;
            &SILENCE_PCM
        };

        let escape_frame;
        let alac_frame: &[u8] = if cfg.uncompressed_alac {
            let samples: Vec<i16> = pcm
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            escape_frame = build_uncompressed_alac_frame(&samples);
            &escape_frame
        } else {
            let n = encoder.encode(&input_format, pcm, &mut alac_buf);
            &alac_buf[..n]
        };

        let pkt_bytes = build_rtp_packet(
            seq,
            rtptime,
            cfg.ssrc,
            packet_count == 0,
            alac_frame,
            &cfg.cipher,
        );

        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }

        if let Err(e) = cfg.audio_socket.send_to(&pkt_bytes, cfg.receiver_addr) {
            warn!("AirPlay RTP send failed: {}", e);
            // Receiver disappeared — give up; the session manager
            // will notice and tear down.
            return;
        }
        // Keep a copy so we can retransmit on a resend request.
        cfg.resend.record(seq, &pkt_bytes);

        seq = seq.wrapping_add(1);
        rtptime = rtptime.wrapping_add(FRAMES_PER_PACKET as u32);
        cfg.current_rtptime.store(rtptime, Ordering::Release);
        packet_count += 1;
    }

    info!(
        "AirPlay RTP sender stopped after {} packets ({} s of audio, {} silence-filled)",
        packet_count,
        packet_count * (FRAMES_PER_PACKET as u64) / (WIRE_SAMPLE_RATE as u64),
        silence_packets,
    );
}

/// Append a PcmFrame's raw little-endian PCM bytes to the staging ring,
/// truncated to whole stereo frames so the ring never misaligns.
fn append_frame_bytes(ring: &mut Vec<u8>, frame: &PcmFrame) {
    let bytes = &**frame.0;
    ring.extend_from_slice(&bytes[..bytes.len() & !3]);
}

/// Build a complete RTP packet: 12-byte header + (optionally encrypted) payload.
fn build_rtp_packet(
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    is_first_packet: bool,
    alac_frame: &[u8],
    cipher: &Cipher,
) -> Vec<u8> {
    let mut pkt = vec![0u8; 12 + alac_frame.len()];

    pkt[0] = RTP_V2;
    let marker = if is_first_packet { 0x80 } else { 0x00 };
    pkt[1] = marker | RTP_PT_AUDIO;
    BigEndian::write_u16(&mut pkt[2..4], seq);
    BigEndian::write_u32(&mut pkt[4..8], timestamp);
    BigEndian::write_u32(&mut pkt[8..12], ssrc);

    pkt[12..].copy_from_slice(alac_frame);
    cipher.encrypt_payload_in_place(&mut pkt[12..]);
    pkt
}

/// Bind a single UDP socket to the same local IP we'll advertise in
/// SDP. Lets the OS pick the port. Used for the audio, control, and
/// timing sockets — each session needs three.
pub fn bind_udp(local_ip: IpAddr) -> Result<UdpSocket> {
    let sock = UdpSocket::bind(SocketAddr::new(local_ip, 0))
        .with_context(|| format!("bind UDP on {}", local_ip))?;
    sock.set_nonblocking(false)?;
    Ok(sock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_layout() {
        let cipher = Cipher::None;
        // Empty ALAC frame so we can inspect the header byte-by-byte.
        let pkt = build_rtp_packet(0x1234, 0xABCD_0123, 0xDEAD_BEEF, true, &[], &cipher);
        assert_eq!(pkt.len(), 12);
        assert_eq!(pkt[0], 0x80); // V=2 P=0 X=0 CC=0
        assert_eq!(pkt[1], 0x80 | 0x60); // marker + PT=96
        assert_eq!(&pkt[2..4], &[0x12, 0x34]);
        assert_eq!(&pkt[4..8], &[0xAB, 0xCD, 0x01, 0x23]);
        assert_eq!(&pkt[8..12], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn rtp_subsequent_packets_have_no_marker() {
        let cipher = Cipher::None;
        let pkt = build_rtp_packet(0, 0, 0, false, &[], &cipher);
        assert_eq!(pkt[1], 0x60); // PT=96, no marker
    }

    #[test]
    fn rtp_unencrypted_payload_is_passed_through() {
        let cipher = Cipher::None;
        let payload = [0xAA, 0xBB, 0xCC, 0xDD];
        let pkt = build_rtp_packet(0, 0, 0, false, &payload, &cipher);
        assert_eq!(&pkt[12..], &payload);
    }

    /// Generate a deterministic full-scale-ish stereo test signal.
    fn test_signal(frames: usize) -> Vec<i16> {
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f32 / WIRE_SAMPLE_RATE as f32;
            let l = ((t * 440.0 * std::f32::consts::TAU).sin() * 12000.0) as i16;
            let r = ((t * 554.37 * std::f32::consts::TAU).sin() * 9000.0) as i16;
            samples.push(l);
            samples.push(r);
        }
        samples
    }

    /// The load-bearing codec test: frames from Apple's encoder must
    /// decode losslessly under the exact fmtp parameters our ANNOUNCE
    /// advertises (`352 0 16 40 10 14 2 255 0 0 44100`). Uses the
    /// independent `alac` decoder crate as the reference receiver.
    #[test]
    fn compressed_alac_round_trips_under_announced_fmtp() {
        let output_format =
            FormatDescription::alac(WIRE_SAMPLE_RATE as f64, FRAMES_PER_PACKET as u32, 2);
        let input_format = FormatDescription::pcm::<i16>(WIRE_SAMPLE_RATE as f64, 2);
        let mut encoder = AlacEncoder::new(&output_format);
        let mut alac_buf = vec![0u8; output_format.max_packet_size()];

        let stream_info =
            alac::StreamInfo::from_sdp_format_parameters("352 0 16 40 10 14 2 255 0 0 44100")
                .expect("our fmtp line parses as ALAC stream info");
        let mut decoder = alac::Decoder::new(stream_info);

        // Multiple packets so the encoder's retained predictor state is
        // exercised across frames, exactly as in a live session.
        let signal = test_signal(FRAMES_PER_PACKET * 5);
        for chunk in signal.chunks_exact(FRAMES_PER_PACKET * 2) {
            let mut pcm_bytes = Vec::with_capacity(chunk.len() * 2);
            for s in chunk {
                pcm_bytes.extend_from_slice(&s.to_le_bytes());
            }
            let n = encoder.encode(&input_format, &pcm_bytes, &mut alac_buf);
            assert!(n > 0, "encoder produced an empty frame");

            let mut out = vec![0i16; FRAMES_PER_PACKET * 2];
            let decoded = decoder
                .decode_packet(&alac_buf[..n], &mut out)
                .expect("reference decoder accepts our frame");
            assert_eq!(decoded, chunk, "ALAC round-trip must be lossless");
        }
    }

    /// Silence must also encode into small valid frames (the silence-fill
    /// path sends these whenever the upstream source stalls).
    #[test]
    fn compressed_alac_silence_frame_is_small_and_valid() {
        let output_format =
            FormatDescription::alac(WIRE_SAMPLE_RATE as f64, FRAMES_PER_PACKET as u32, 2);
        let input_format = FormatDescription::pcm::<i16>(WIRE_SAMPLE_RATE as f64, 2);
        let mut encoder = AlacEncoder::new(&output_format);
        let mut alac_buf = vec![0u8; output_format.max_packet_size()];

        let pcm_bytes = vec![0u8; FRAMES_PER_PACKET * 2 * 2];
        let n = encoder.encode(&input_format, &pcm_bytes, &mut alac_buf);
        assert!(n > 0);
        assert!(
            n < 100,
            "352 frames of digital silence should compress to a few dozen bytes, got {}",
            n
        );

        let stream_info =
            alac::StreamInfo::from_sdp_format_parameters("352 0 16 40 10 14 2 255 0 0 44100")
                .unwrap();
        let mut decoder = alac::Decoder::new(stream_info);
        let mut out = vec![0i16; FRAMES_PER_PACKET * 2];
        let decoded = decoder.decode_packet(&alac_buf[..n], &mut out).unwrap();
        assert!(decoded.iter().all(|&s| s == 0));
        assert_eq!(decoded.len(), FRAMES_PER_PACKET * 2);
    }
}
