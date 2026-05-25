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
//! sending. This keeps the receiver's jitter buffer from over- or under-
//! flowing as long as the source produces audio at real time (which the
//! upstream silence injector guarantees even during idle).
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
use crate::http_server::PcmFrame;
use crate::WIRE_SAMPLE_RATE;

/// Sample frames per RTP packet — matches the ANNOUNCE `fmtp` value.
pub const FRAMES_PER_PACKET: usize = 352;

/// RTP payload type. RAOP fixes this at 96 (dynamic, first in range).
const RTP_PT_AUDIO: u8 = 0x60;

/// RTP version 2 in the top two bits of byte 0.
const RTP_V2: u8 = 0x80;

/// Bytes per stereo sample frame in our wire format (16-bit L + R).
const BYTES_PER_FRAME: usize = 4;

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
        "AirPlay RTP sender starting → {} (initial seq={}, rtptime={})",
        cfg.receiver_addr, cfg.initial_seq, cfg.initial_rtptime,
    );

    let mut seq = cfg.initial_seq;
    let mut rtptime = cfg.initial_rtptime;
    let mut packet_count: u64 = 0;
    let mut sample_ring: Vec<i16> = Vec::with_capacity(FRAMES_PER_PACKET * BYTES_PER_FRAME);

    let start = Instant::now();
    let packet_duration = Duration::from_nanos(
        ((FRAMES_PER_PACKET as u64) * 1_000_000_000) / (WIRE_SAMPLE_RATE as u64),
    );

    loop {
        if cfg.stop_flag.load(Ordering::Acquire) {
            debug!("AirPlay RTP sender: stop flag set, exiting");
            break;
        }

        // Pull all currently-available frames into the ring. Block
        // briefly for the first frame to keep the loop from spinning
        // while we wait for the producer.
        let blocking_timeout = Duration::from_millis(50);
        match cfg.samples_rx.recv_timeout(blocking_timeout) {
            Ok(frame) => {
                append_samples_from_frame(&mut sample_ring, &frame);
                // Drain anything else queued without blocking.
                loop {
                    match cfg.samples_rx.try_recv() {
                        Ok(f) => append_samples_from_frame(&mut sample_ring, &f),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            info!("AirPlay RTP sender: PCM source disconnected, exiting");
                            return;
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // No fresh audio. Don't burn CPU; loop again.
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                info!("AirPlay RTP sender: PCM source disconnected, exiting");
                return;
            }
        }

        // Slice 352-frame packets out of the ring while we have enough.
        let mut packets_this_round = 0u32;
        while sample_ring.len() >= FRAMES_PER_PACKET * 2 {
            let pkt_samples: Vec<i16> = sample_ring
                .drain(..FRAMES_PER_PACKET * 2)
                .collect();
            let alac_frame = build_uncompressed_alac_frame(&pkt_samples);
            let pkt_bytes = build_rtp_packet(
                seq,
                rtptime,
                cfg.ssrc,
                packet_count == 0,
                &alac_frame,
                &cfg.cipher,
            );

            // Pace to wall-clock so we don't outrun the receiver's
            // jitter buffer. We compute the absolute deadline from
            // packet_count so accumulated jitter doesn't drift.
            let deadline =
                start + packet_duration.saturating_mul((packet_count + 1) as u32);
            let now = Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            }

            if let Err(e) = cfg.audio_socket.send_to(&pkt_bytes, cfg.receiver_addr) {
                warn!("AirPlay RTP send failed: {}", e);
                // Receiver disappeared — give up; the session manager
                // will notice via TCP keepalive and tear down.
                return;
            }

            seq = seq.wrapping_add(1);
            rtptime = rtptime.wrapping_add(FRAMES_PER_PACKET as u32);
            cfg.current_rtptime.store(rtptime, Ordering::Release);
            packet_count += 1;
            packets_this_round += 1;
            if packets_this_round > 32 {
                // Avoid starving the inbound channel reader.
                break;
            }
        }
    }

    info!(
        "AirPlay RTP sender stopped after {} packets ({} s of audio)",
        packet_count,
        packet_count * (FRAMES_PER_PACKET as u64) / (WIRE_SAMPLE_RATE as u64),
    );
}

/// Re-interpret the LE-byte PcmFrame buffer as i16 samples and append
/// them to the ring. Source format is L0_lo, L0_hi, R0_lo, R0_hi, ...
fn append_samples_from_frame(ring: &mut Vec<i16>, frame: &PcmFrame) {
    let bytes = &**frame.0;
    let n_samples = bytes.len() / 2;
    let start = ring.len();
    ring.resize(start + n_samples, 0);
    for i in 0..n_samples {
        let lo = bytes[i * 2];
        let hi = bytes[i * 2 + 1];
        ring[start + i] = i16::from_le_bytes([lo, hi]);
    }
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
}
