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
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
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
/// the shared PTP clock (the receiver's grandmaster time we follow),
/// expressed as a 32.32 fixed-point value in the PTP timescale. Used for
/// HomePods and other receivers that mandate IEEE-1588 timing.
pub fn spawn_sync_sender_ptp(
    control_socket: UdpSocket,
    receiver_addr: SocketAddr,
    current_rtptime: Arc<AtomicU32>,
    latency_samples: u32,
    ptp: Arc<crate::airplay::ap2_ptp::PtpClock>,
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
        move || ns_to_fixed_32_32(ptp.master_now_ns()),
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
    fn fixed_32_32_encodes_seconds_and_fraction() {
        assert_eq!(ns_to_fixed_32_32(0), 0);
        assert_eq!(ns_to_fixed_32_32(1_000_000_000), 1u64 << 32);
        // Half a second ≈ 0x8000_0000 in the fractional word.
        let half = ns_to_fixed_32_32(1_500_000_000);
        assert_eq!(half >> 32, 1);
        assert!(((half & 0xFFFF_FFFF) as i64 - 0x8000_0000i64).abs() < 4);
        // Negative clamps to zero rather than wrapping.
        assert_eq!(ns_to_fixed_32_32(-5), 0);
    }
}

/// Convert a nanosecond count to a 32.32 fixed-point seconds value
/// (seconds in the high 32 bits, fractional seconds in the low 32). Used
/// to format the PTP clock for the sync packet's network-time field.
fn ns_to_fixed_32_32(ns: i64) -> u64 {
    let ns = ns.max(0) as u64;
    let secs = ns / 1_000_000_000;
    let frac = ((ns % 1_000_000_000) << 32) / 1_000_000_000;
    (secs << 32) | frac
}
