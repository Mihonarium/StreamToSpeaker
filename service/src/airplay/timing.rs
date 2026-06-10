//! Sync + timing packet handlers.
//!
//! Two concurrent UDP responsibilities:
//!
//! ## Timing channel (required)
//!
//! The receiver periodically sends a 32-byte timing **request** to
//! our `timing_port`. We must echo a 32-byte **response** within a
//! sensible window — failure to respond causes most receivers to
//! drift audibly or drop the session within ~10 seconds.
//!
//! Request layout (32 bytes):
//!
//! ```text
//!  0..1  : 0x80                 # V=2
//!  1..2  : 0xD2                 # M=1, PT=82=0x52 (timing request)
//!  2..4  : sequence (BE u16)
//!  4..8  : zero padding
//!  8..16 : zero / "origin"
//! 16..24 : zero / "received"
//! 24..32 : NTP timestamp of when the receiver sent this  (echo back)
//! ```
//!
//! Response layout (32 bytes):
//!
//! ```text
//!  0..1  : 0x80
//!  1..2  : 0xD3                 # M=1, PT=83=0x53 (timing response)
//!  2..4  : 0x0007               # constant
//!  4..8  : zero padding
//!  8..16 : NTP "reference"   ← copy bytes 24..32 of request
//! 16..24 : NTP "received"    ← capture immediately at recv
//! 24..32 : NTP "transmit"    ← capture immediately before send
//! ```
//!
//! ## Sync channel (advisory)
//!
//! We send a sync packet to the receiver's `control_port` roughly
//! once a second to keep its drift estimator anchored. Most receivers
//! cope without this in the short term but eventually re-buffer or
//! glitch without it.
//!
//! Sync packet layout (20 bytes):
//!
//! ```text
//!  0..1  : 0x80 (or 0x90 on first packet, X bit set)
//!  1..2  : 0xD4                 # M=1, PT=84=0x54
//!  2..4  : 0x0007               # constant
//!  4..8  : RTP timestamp − latency (BE u32) — "now should be playing"
//!  8..16 : NTP timestamp of now
//! 16..20 : current RTP timestamp (BE u32) — "now being sent"
//! ```

use byteorder::{BigEndian, ByteOrder};
use log::{debug, warn};
use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// NTP epoch offset — seconds between 1900-01-01 and 1970-01-01.
const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

/// Sequence value RAOP uses for sync + timing responses. The receiver
/// only checks this on timing requests it sends; for our responses
/// the constant 7 is what every reference implementation uses.
const RAOP_FIXED_SEQ: u16 = 7;

/// Convert `SystemTime::now()` to a 64-bit NTP timestamp (seconds
/// since 1900-01-01 in the high 32 bits, fractional seconds in the
/// low 32 bits). Saturates on broken clocks.
pub fn ntp_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() + NTP_EPOCH_OFFSET;
    let frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// Spawn the timing responder. Owns the timing socket. Exits when
/// `stop_flag` is set OR when the socket errors persistently.
pub fn spawn_timing_responder(
    timing_socket: UdpSocket,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
) -> std::io::Result<thread::JoinHandle<()>> {
    timing_socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    thread::Builder::new()
        .name(format!("stream-to-speaker-airplay-timing:{}", receiver_name))
        .spawn(move || {
            let mut buf = [0u8; 64];
            while !stop_flag.load(Ordering::Acquire) {
                match timing_socket.recv_from(&mut buf) {
                    Ok((n, peer)) if n >= 32 => {
                        let received_ntp = ntp_now();
                        handle_timing_request(
                            &buf[..n],
                            peer,
                            received_ntp,
                            &timing_socket,
                        );
                    }
                    Ok((n, _)) => {
                        debug!("AirPlay timing: short packet ({} bytes), ignoring", n);
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(e) => {
                        warn!("AirPlay timing recv error: {}", e);
                    }
                }
            }
            debug!("AirPlay timing responder: exiting");
        })
}

fn handle_timing_request(req: &[u8], peer: SocketAddr, received_ntp: u64, sock: &UdpSocket) {
    // Sanity: byte 0 should be 0x80, byte 1 should be 0xD2 (timing
    // request, M=1 PT=82). We accept anything as long as it's 32+ bytes
    // — some receivers send slightly off values.
    let mut resp = [0u8; 32];
    resp[0] = 0x80;
    resp[1] = 0xD3;
    BigEndian::write_u16(&mut resp[2..4], RAOP_FIXED_SEQ);
    // bytes 4..8 stay zero (padding)
    // reference_time: echo the request's bytes 24..32 (the receiver's
    // "transmit" timestamp).
    resp[8..16].copy_from_slice(&req[24..32]);
    // received_time: when we recv'd
    BigEndian::write_u64(&mut resp[16..24], received_ntp);
    // transmit_time: now (just before send)
    let transmit_ntp = ntp_now();
    BigEndian::write_u64(&mut resp[24..32], transmit_ntp);

    if let Err(e) = sock.send_to(&resp, peer) {
        warn!("AirPlay timing: failed to reply to {}: {}", peer, e);
    }
}

/// Spawn the sync packet sender. Sends one 20-byte sync packet per
/// second to the receiver's `control_port` until `stop_flag` is set.
///
/// Latency is the receiver-advertised buffer depth in samples
/// (typically 11025 = 250 ms at 44.1 kHz) — we use it to compute the
/// "now should be playing" anchor timestamp.
pub fn spawn_sync_sender(
    control_socket: UdpSocket,
    receiver_addr: SocketAddr,
    current_rtptime: Arc<AtomicU32>,
    latency_samples: u32,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
) -> std::io::Result<thread::JoinHandle<()>> {
    // NTP timeline: the network-time field carries our local NTP clock.
    spawn_sync_sender_inner(
        control_socket,
        receiver_addr,
        current_rtptime,
        latency_samples,
        stop_flag,
        receiver_name,
        ntp_now,
    )
}

/// PTP variant: the network-time field of the 0xD4 sync packet carries
/// the **PTP grandmaster clock we ourselves serve** (the receiver follows
/// our clock — see [`crate::airplay::ap2_ptp`]), with the NTP 1900-epoch
/// delta added to the seconds. That matches OwnTone exactly: its sync
/// packets are `CLOCK_MONOTONIC + NTP_EPOCH_DELTA` while its PTP daemon
/// serves raw `CLOCK_MONOTONIC`, so the receiver can equate
/// "sync time − epoch delta" with the PTP timeline it is locked to.
pub fn spawn_sync_sender_ptp(
    control_socket: UdpSocket,
    receiver_addr: SocketAddr,
    current_rtptime: Arc<AtomicU32>,
    latency_samples: u32,
    ptp_clock: Arc<crate::airplay::ap2_ptp::PtpMasterClock>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
) -> std::io::Result<thread::JoinHandle<()>> {
    spawn_sync_sender_inner(
        control_socket,
        receiver_addr,
        current_rtptime,
        latency_samples,
        stop_flag,
        receiver_name,
        move || ptp_ns_to_ntp_fixed(ptp_clock.now_ns()),
    )
}

fn spawn_sync_sender_inner<F>(
    control_socket: UdpSocket,
    receiver_addr: SocketAddr,
    current_rtptime: Arc<AtomicU32>,
    latency_samples: u32,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
    network_time: F,
) -> std::io::Result<thread::JoinHandle<()>>
where
    F: Fn() -> u64 + Send + 'static,
{
    thread::Builder::new()
        .name(format!("stream-to-speaker-airplay-sync:{}", receiver_name))
        .spawn(move || {
            // Brief delay before first sync packet so the RTP stream is
            // already flowing — sending a sync ahead of any audio
            // confuses some receivers.
            thread::sleep(Duration::from_millis(500));
            let mut first = true;
            while !stop_flag.load(Ordering::Acquire) {
                let net_time = network_time();
                let cur_rtp = current_rtptime.load(Ordering::Acquire);
                let anchor_rtp = cur_rtp.wrapping_sub(latency_samples);

                let mut pkt = [0u8; 20];
                pkt[0] = if first { 0x90 } else { 0x80 };
                pkt[1] = 0xD4;
                BigEndian::write_u16(&mut pkt[2..4], RAOP_FIXED_SEQ);
                BigEndian::write_u32(&mut pkt[4..8], anchor_rtp);
                BigEndian::write_u64(&mut pkt[8..16], net_time);
                BigEndian::write_u32(&mut pkt[16..20], cur_rtp);
                first = false;

                if let Err(e) = control_socket.send_to(&pkt, receiver_addr) {
                    warn!(
                        "AirPlay sync send to {} failed: {}",
                        receiver_addr, e
                    );
                }

                // Sleep ~1 s, but in 100 ms slices so stop_flag is
                // observed quickly during shutdown.
                for _ in 0..10 {
                    if stop_flag.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
            debug!("AirPlay sync sender: exiting");
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptp_ntp_fixed_applies_epoch_and_fraction() {
        // Zero on the PTP timeline = exactly the NTP epoch delta seconds.
        assert_eq!(ptp_ns_to_ntp_fixed(0), NTP_EPOCH_OFFSET << 32);
        assert_eq!(ptp_ns_to_ntp_fixed(1_000_000_000), (NTP_EPOCH_OFFSET + 1) << 32);
        // Half a second ≈ 0x8000_0000 in the fractional word.
        let half = ptp_ns_to_ntp_fixed(1_500_000_000);
        assert_eq!(half >> 32, NTP_EPOCH_OFFSET + 1);
        assert!(((half & 0xFFFF_FFFF) as i64 - 0x8000_0000i64).abs() < 4);
    }

    #[test]
    fn resend_buffer_records_evicts_and_fetches() {
        let rb = ResendBuffer::new(3);
        rb.record(10, &[0xAA]);
        rb.record(11, &[0xBB]);
        rb.record(12, &[0xCC]);
        assert_eq!(rb.get(10).as_deref(), Some(&[0xAA][..]));
        // Fourth push evicts seq 10.
        rb.record(13, &[0xDD]);
        assert_eq!(rb.get(10), None);
        assert_eq!(rb.get(13).as_deref(), Some(&[0xDD][..]));
        assert_eq!(rb.get(999), None);
    }

    #[test]
    fn resend_response_wraps_original_packet() {
        let original = [0x80, 0x60, 0x12, 0x34, 0xDE, 0xAD];
        let resp = build_resend_response(0x1234, &original);
        assert_eq!(resp[0], 0x80);
        assert_eq!(resp[1], 0xD6);
        assert_eq!(&resp[2..4], &[0x12, 0x34]); // echoed seq
        assert_eq!(&resp[4..], &original); // original packet appended verbatim
    }
}

/// Convert a PTP-timeline nanosecond count to the sync packet's NTP-style
/// 32.32 field: seconds (plus the 1900 epoch delta) in the high 32 bits,
/// fractional seconds in the low 32. Mirrors OwnTone's `timespec_to_ntp`
/// applied to its monotonic PTP clock.
fn ptp_ns_to_ntp_fixed(ns: u64) -> u64 {
    let secs = ns / 1_000_000_000 + NTP_EPOCH_OFFSET;
    let frac = ((ns % 1_000_000_000) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

// ---------------------------------------------------------------------------
// Retransmit / resend (control channel)
// ---------------------------------------------------------------------------

/// Largest run of packets we'll re-send for a single request — a sanity
/// cap so a malformed `count` can't make us flood the receiver.
const MAX_RESEND_RUN: u16 = 128;

/// Ring of recently-sent audio packets keyed by RTP sequence number, so
/// we can answer the receiver's retransmit (resend) requests when Wi-Fi
/// drops a packet. Stores the full on-wire bytes (RTP header + payload),
/// which works for RAOP (plain/AES) and AirPlay 2 (ChaCha-sealed) alike.
pub struct ResendBuffer {
    inner: Mutex<VecDeque<(u16, Vec<u8>)>>,
    cap: usize,
}

impl ResendBuffer {
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
        })
    }

    /// Record a just-sent packet. Evicts the oldest once at capacity.
    pub fn record(&self, seq: u16, packet: &[u8]) {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.cap {
            q.pop_front();
        }
        q.push_back((seq, packet.to_vec()));
    }

    /// Fetch a previously-sent packet by sequence number, newest first.
    pub fn get(&self, seq: u16) -> Option<Vec<u8>> {
        let q = self.inner.lock().unwrap();
        q.iter().rev().find(|(s, _)| *s == seq).map(|(_, p)| p.clone())
    }
}

/// Wrap an original audio packet in the 4-byte RAOP resend-response
/// header (`0x80 0xD6 <orig-seq BE>`) the receiver expects on the
/// control channel.
fn build_resend_response(seq: u16, original_packet: &[u8]) -> Vec<u8> {
    let mut resp = Vec::with_capacity(4 + original_packet.len());
    resp.push(0x80);
    resp.push(0xD6); // M=1, PT=86 (resend response)
    resp.extend_from_slice(&seq.to_be_bytes());
    resp.extend_from_slice(original_packet);
    resp
}

/// Spawn the retransmit responder. Listens on the control socket for
/// resend *requests* (`0x80 0xD5`: first-missing-seq + run length) and
/// re-sends matching buffered packets to the receiver's control port.
///
/// The control socket is shared with the sync sender via `try_clone`
/// (the sync sender only writes; this thread reads + writes).
pub fn spawn_resend_responder(
    control_socket: UdpSocket,
    receiver_control_addr: SocketAddr,
    resend: Arc<ResendBuffer>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
) -> std::io::Result<thread::JoinHandle<()>> {
    control_socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    thread::Builder::new()
        .name(format!("stream-to-speaker-airplay-resend:{}", receiver_name))
        .spawn(move || {
            let mut buf = [0u8; 64];
            while !stop_flag.load(Ordering::Acquire) {
                match control_socket.recv_from(&mut buf) {
                    // Resend request: 0x80 0xD5, seq(2), first(2), count(2).
                    Ok((n, _)) if n >= 8 && buf[1] == 0xD5 => {
                        let first = BigEndian::read_u16(&buf[4..6]);
                        let count = BigEndian::read_u16(&buf[6..8]).min(MAX_RESEND_RUN);
                        let mut sent = 0u16;
                        for i in 0..count {
                            let seq = first.wrapping_add(i);
                            if let Some(pkt) = resend.get(seq) {
                                let resp = build_resend_response(seq, &pkt);
                                if control_socket.send_to(&resp, receiver_control_addr).is_ok() {
                                    sent += 1;
                                }
                            }
                        }
                        debug!(
                            "AirPlay resend: req first={} count={} → re-sent {}",
                            first, count, sent
                        );
                    }
                    // Anything else on the control socket (e.g. the
                    // receiver echoing sync) we ignore.
                    Ok(_) => {}
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(e) => {
                        warn!("AirPlay resend recv error: {}", e);
                    }
                }
            }
            debug!("AirPlay resend responder: exiting");
        })
}
