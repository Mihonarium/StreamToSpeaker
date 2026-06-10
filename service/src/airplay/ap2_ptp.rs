//! IEEE-1588 (PTPv2) timing for AirPlay 2 — the path HomePods / Sonos expect.
//!
//! ## The sender is the grandmaster
//!
//! In AirPlay 2 the *sender* owns the timeline: it runs a small PTP master
//! and the receiver follows the sender's clock (this is how iOS → HomePod
//! works, how shairport-sync/nqptp receive, and how OwnTone's `libairptp`
//! sends — `ptpd_slave_add()` registers the *receiver* as a slave of the
//! sender's daemon). Getting this backwards (following the receiver) leaves
//! the receiver with no clock to bind to: it accepts and decrypts audio but
//! never schedules it — session shows "playing", output is silence.
//!
//! Per registered peer we send unicast:
//!   * **Announce** → port 320, ~1 s cadence
//!   * **Sync** (two-step) → port 319, 125 ms cadence, followed by
//!     **Follow_Up** → port 320 carrying the precise origin timestamp
//!   * **Delay_Resp** → port 320, answering the peer's Delay_Req (→ our 319)
//!
//! Wire details mirror `libairptp` (OwnTone), verified against Sonos and
//! HomePod: flags `UNICAST|TIMESCALE` (+ `TWO_STEP` on Sync), Announce
//! grandmaster fields priority1/2 = 128, clockClass 0x06, clockAccuracy
//! 0x21, offsetScaledLogVariance 0x436A, timeSource 0x20, sourcePortIdentity
//! port number 0x8005.
//!
//! ## The clock
//!
//! The timeline we serve is a **monotonic** clock (libairptp uses
//! `CLOCK_MONOTONIC`; we use `Instant` from session start) — *not* wall
//! time. The `0xD4` audio sync packet must carry this same timeline with
//! the NTP 1900-epoch delta added to the seconds (that is exactly what
//! OwnTone's `rtp_sync_packet_next` does), so the receiver can equate
//! "sync-packet time − 0x83AA7E80" with the PTP clock it follows.
//!
//! On Unix, binding 319/320 needs `CAP_NET_BIND_SERVICE`; on Windows no
//! special privilege is required. Because we transmit first from both
//! ports, Windows Firewall's stateful UDP handling admits the receiver's
//! replies (Delay_Req) without dedicated inbound rules.

use anyhow::{Context, Result};
use log::{debug, info, warn};
use rand::Rng;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const PTP_EVENT_PORT: u16 = 319;
pub const PTP_GENERAL_PORT: u16 = 320;

const PTP_VERSION: u8 = 2;
const HEADER_LEN: usize = 34;

// Message types (low nibble of byte 0).
const MSG_SYNC: u8 = 0x0;
#[cfg(test)]
const MSG_DELAY_REQ: u8 = 0x1;
const MSG_FOLLOW_UP: u8 = 0x8;
const MSG_DELAY_RESP: u8 = 0x9;
const MSG_ANNOUNCE: u8 = 0xB;

// Header flags (big-endian u16 at bytes 6..8).
const FLAG_TWO_STEP: u16 = 1 << 9;
const FLAG_UNICAST: u16 = 1 << 10;
const FLAG_TIMESCALE: u16 = 1 << 3;
const FLAGS_GENERAL: u16 = FLAG_UNICAST | FLAG_TIMESCALE; // 0x0408
const FLAGS_SYNC: u16 = FLAGS_GENERAL | FLAG_TWO_STEP; // 0x0608

// logMessageInterval per message kind.
const LOG_INTERVAL_ANNOUNCE: i8 = 0; // 1 s
const LOG_INTERVAL_SYNC: i8 = -3; // 125 ms
const LOG_INTERVAL_DELAY_RESP: i8 = 0x7f;

// sourcePortIdentity port number — Apple stacks use 0x8005 (libairptp
// hardcodes it); receivers key on it being stable, not on the value.
const PORT_NUMBER: [u8; 2] = [0x80, 0x05];

const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_INTERVAL: Duration = Duration::from_millis(125);

/// The monotonic timeline we serve as PTP grandmaster. Receivers follow
/// this clock; the `0xD4` audio sync packets must be stamped from the
/// same instance (see [`crate::airplay::timing::spawn_sync_sender_ptp`]).
pub struct PtpMasterClock {
    start: Instant,
}

impl PtpMasterClock {
    fn new() -> Arc<Self> {
        Arc::new(Self { start: Instant::now() })
    }

    /// Nanoseconds on our PTP timeline (monotonic since session start).
    pub fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

/// Handle to a running PTP master.
pub struct PtpMaster {
    pub clock: Arc<PtpMasterClock>,
    /// Our 8-byte clock identity as a u64 — goes BE into every PTP header
    /// and (as int64) into the RTSP `timingPeerInfo.ClockID`.
    pub clock_id: u64,
    /// UUID string for the RTSP `timingPeerInfo.ID` field.
    pub clock_uuid: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PtpMaster {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PtpMaster {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Start the PTP master serving `receiver_ip`. Binds the event (319) and
/// general (320) ports — one session at a time owns them — and starts the
/// announce/sync transmit loop immediately so the clock is live before the
/// RTSP SETUP that advertises it.
pub fn spawn_ptp_master(receiver_ip: IpAddr, local_ip: IpAddr, receiver_name: String) -> Result<PtpMaster> {
    let event = bind_ptp(local_ip, PTP_EVENT_PORT).context("bind PTP event :319")?;
    let general = bind_ptp(local_ip, PTP_GENERAL_PORT).context("bind PTP general :320")?;
    event.set_read_timeout(Some(Duration::from_millis(25)))?;
    general.set_nonblocking(true)?;

    let mut rng = rand::thread_rng();
    let clock_id: u64 = rng.gen();
    let clock_uuid = format_uuid(rng.gen());

    let clock = PtpMasterClock::new();
    let stop = Arc::new(AtomicBool::new(false));

    let clock_t = clock.clone();
    let stop_t = stop.clone();
    let handle = thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-ptp:{}", receiver_name))
        .spawn(move || {
            run_master(event, general, receiver_ip, clock_id, clock_t, stop_t);
        })?;

    info!(
        "AirPlay 2 PTP: serving as grandmaster for {} (clock_id={:#018x})",
        receiver_ip, clock_id
    );
    Ok(PtpMaster { clock, clock_id, clock_uuid, stop, handle: Some(handle) })
}

fn bind_ptp(local_ip: IpAddr, port: u16) -> Result<UdpSocket> {
    match UdpSocket::bind(SocketAddr::new(local_ip, port)) {
        Ok(s) => Ok(s),
        Err(e) => {
            warn!(
                "PTP: bind {} failed ({}); using ephemeral port — receiver will likely not lock",
                port, e
            );
            UdpSocket::bind(SocketAddr::new(local_ip, 0)).map_err(Into::into)
        }
    }
}

fn run_master(
    event: UdpSocket,
    general: UdpSocket,
    receiver_ip: IpAddr,
    clock_id: u64,
    clock: Arc<PtpMasterClock>,
    stop: Arc<AtomicBool>,
) {
    let event_dst = SocketAddr::new(receiver_ip, PTP_EVENT_PORT);
    let general_dst = SocketAddr::new(receiver_ip, PTP_GENERAL_PORT);

    let mut announce_seq: u16 = 0;
    let mut sync_seq: u16 = 0;
    let mut last_announce: Option<Instant> = None;
    let mut last_sync: Option<Instant> = None;
    let mut delay_resps: u64 = 0;
    let mut buf = [0u8; 256];

    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();

        if last_announce.map_or(true, |t| now.duration_since(t) >= ANNOUNCE_INTERVAL) {
            let pkt = build_announce(clock_id, announce_seq);
            let _ = general.send_to(&pkt, general_dst);
            announce_seq = announce_seq.wrapping_add(1);
            last_announce = Some(now);
        }

        if last_sync.map_or(true, |t| now.duration_since(t) >= SYNC_INTERVAL) {
            // Two-step: Sync carries a coarse origin, the Follow_Up sent
            // right behind it carries the precise origin timestamp.
            let coarse = clock.now_ns();
            let sync = build_sync(clock_id, sync_seq, coarse);
            let _ = event.send_to(&sync, event_dst);
            let precise = clock.now_ns();
            let fup = build_follow_up(clock_id, sync_seq, precise);
            let _ = general.send_to(&fup, general_dst);
            sync_seq = sync_seq.wrapping_add(1);
            last_sync = Some(now);
        }

        // Event port: the receiver's Delay_Req arrives here. The 25 ms read
        // timeout doubles as the loop tick.
        match event.recv_from(&mut buf) {
            Ok((n, src)) => {
                let t_recv = clock.now_ns();
                if let Some(h) = parse_header(&buf[..n]) {
                    if h.msg_type == 0x1 {
                        let resp = build_delay_resp(
                            clock_id,
                            h.sequence_id,
                            t_recv,
                            &h.source_port_identity,
                        );
                        let _ = general.send_to(&resp, SocketAddr::new(src.ip(), PTP_GENERAL_PORT));
                        delay_resps += 1;
                        if delay_resps == 1 {
                            info!("AirPlay 2 PTP: receiver {} is exchanging Delay_Req — clock lock in progress", src.ip());
                        }
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                debug!("AP2 PTP event recv error: {}", e);
            }
        }

        // General port: drain (the receiver may announce its own clock at
        // lower precedence; with clockClass 6 we win BMCA — ignore).
        while let Ok((_n, _src)) = general.recv_from(&mut buf) {}
    }
    debug!("AirPlay 2 PTP master exiting ({} delay-resps served)", delay_resps);
}

// ---------------------------------------------------------------------------
// Packet codec
// ---------------------------------------------------------------------------

struct PtpHeader {
    msg_type: u8,
    sequence_id: u16,
    source_port_identity: [u8; 10],
}

fn parse_header(buf: &[u8]) -> Option<PtpHeader> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let msg_type = buf[0] & 0x0f;
    if buf[1] & 0x0f != PTP_VERSION {
        return None;
    }
    let sequence_id = u16::from_be_bytes([buf[30], buf[31]]);
    let mut source_port_identity = [0u8; 10];
    source_port_identity.copy_from_slice(&buf[20..30]);
    Some(PtpHeader { msg_type, sequence_id, source_port_identity })
}

fn build_header(
    out: &mut Vec<u8>,
    msg_type: u8,
    flags: u16,
    clock_id: u64,
    seq: u16,
    body_len: usize,
    control: u8,
    log_interval: i8,
) {
    let total = HEADER_LEN + body_len;
    out.push(msg_type & 0x0f); // transportSpecific=0
    out.push(PTP_VERSION & 0x0f);
    out.extend_from_slice(&(total as u16).to_be_bytes()); // messageLength
    out.push(0); // domainNumber
    out.push(0); // reserved
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&0i64.to_be_bytes()); // correctionField
    out.extend_from_slice(&0u32.to_be_bytes()); // reserved
    out.extend_from_slice(&clock_id.to_be_bytes()); // clockIdentity (8)
    out.extend_from_slice(&PORT_NUMBER); // sourcePortNumber
    out.extend_from_slice(&seq.to_be_bytes()); // sequenceId
    out.push(control); // controlField
    out.push(log_interval as u8); // logMessageInterval
}

fn build_announce(clock_id: u64, seq: u16) -> Vec<u8> {
    // Body: originTimestamp(10) currentUtcOffset(2) reserved(1)
    // grandmasterPriority1(1) grandmasterClockQuality(4)
    // grandmasterPriority2(1) grandmasterIdentity(8) stepsRemoved(2)
    // timeSource(1) = 30 bytes.
    let mut out = Vec::with_capacity(HEADER_LEN + 30);
    build_header(&mut out, MSG_ANNOUNCE, FLAGS_GENERAL, clock_id, seq, 30, 0x05, LOG_INTERVAL_ANNOUNCE);
    out.extend_from_slice(&[0u8; 10]); // originTimestamp
    out.extend_from_slice(&0i16.to_be_bytes()); // currentUtcOffset
    out.push(0); // reserved
    // Grandmaster fields — libairptp's exact values: a confident master
    // (clockClass 6 "primary reference") so the receiver's own clock
    // (clockClass 248) loses BMCA and follows us.
    out.push(128); // priority1
    out.push(0x06); // clockClass
    out.push(0x21); // clockAccuracy (100 ns)
    out.extend_from_slice(&0x436Au16.to_be_bytes()); // offsetScaledLogVariance
    out.push(128); // priority2
    out.extend_from_slice(&clock_id.to_be_bytes()); // grandmasterIdentity
    out.extend_from_slice(&0u16.to_be_bytes()); // stepsRemoved
    out.push(0x20); // timeSource = GPS
    out
}

fn build_sync(clock_id: u64, seq: u16, origin_ns: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 10);
    build_header(&mut out, MSG_SYNC, FLAGS_SYNC, clock_id, seq, 10, 0x00, LOG_INTERVAL_SYNC);
    write_timestamp(&mut out, origin_ns);
    out
}

fn build_follow_up(clock_id: u64, seq: u16, origin_ns: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 10);
    build_header(&mut out, MSG_FOLLOW_UP, FLAGS_GENERAL, clock_id, seq, 10, 0x02, LOG_INTERVAL_SYNC);
    write_timestamp(&mut out, origin_ns);
    out
}

fn build_delay_resp(clock_id: u64, seq: u16, receive_ns: u64, requesting_port_identity: &[u8; 10]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 20);
    build_header(&mut out, MSG_DELAY_RESP, FLAGS_GENERAL, clock_id, seq, 20, 0x03, LOG_INTERVAL_DELAY_RESP);
    write_timestamp(&mut out, receive_ns); // receiveTimestamp
    out.extend_from_slice(requesting_port_identity);
    out
}

#[cfg(test)]
fn build_delay_req(clock_id: u64, seq: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 10);
    build_header(&mut out, MSG_DELAY_REQ, FLAG_UNICAST, clock_id, seq, 10, 0x01, 0x7f);
    out.extend_from_slice(&[0u8; 10]);
    out
}

/// Write a 10-byte PTP timestamp (48-bit seconds BE + 32-bit ns BE).
fn write_timestamp(out: &mut Vec<u8>, ns: u64) {
    let secs = ns / 1_000_000_000;
    let nanos = (ns % 1_000_000_000) as u32;
    out.push((secs >> 40) as u8);
    out.push((secs >> 32) as u8);
    out.push((secs >> 24) as u8);
    out.push((secs >> 16) as u8);
    out.push((secs >> 8) as u8);
    out.push(secs as u8);
    out.extend_from_slice(&nanos.to_be_bytes());
}

/// Read a 10-byte PTP timestamp at `off`, returning ns.
#[cfg(test)]
fn read_timestamp(buf: &[u8], off: usize) -> Option<u64> {
    if buf.len() < off + 10 {
        return None;
    }
    let secs = ((buf[off] as u64) << 40)
        | ((buf[off + 1] as u64) << 32)
        | ((buf[off + 2] as u64) << 24)
        | ((buf[off + 3] as u64) << 16)
        | ((buf[off + 4] as u64) << 8)
        | (buf[off + 5] as u64);
    let nanos = u32::from_be_bytes([buf[off + 6], buf[off + 7], buf[off + 8], buf[off + 9]]);
    Some(secs * 1_000_000_000 + nanos as u64)
}

fn format_uuid(bytes: [u8; 16]) -> String {
    let h: Vec<String> = bytes.iter().map(|b| format!("{:02X}", b)).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_carries_type_seq_flags_and_port_identity() {
        let sync = build_sync(0x0102030405060708, 0x1234, 5_000_000_123);
        let h = parse_header(&sync).unwrap();
        assert_eq!(h.msg_type, MSG_SYNC);
        assert_eq!(h.sequence_id, 0x1234);
        // clockIdentity BE + portNumber 0x8005.
        assert_eq!(&h.source_port_identity[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&h.source_port_identity[8..], &PORT_NUMBER);
        // messageLength = header + 10-byte timestamp.
        assert_eq!(u16::from_be_bytes([sync[2], sync[3]]), (HEADER_LEN + 10) as u16);
        // Sync flags: UNICAST | TIMESCALE | TWO_STEP = 0x0608.
        assert_eq!(u16::from_be_bytes([sync[6], sync[7]]), 0x0608);
        // Follow_Up drops TWO_STEP → 0x0408.
        let fup = build_follow_up(1, 1, 0);
        assert_eq!(u16::from_be_bytes([fup[6], fup[7]]), 0x0408);
    }

    #[test]
    fn announce_matches_libairptp_grandmaster_fields() {
        let id = 0xAABBCCDDEEFF0011u64;
        let ann = build_announce(id, 7);
        let h = parse_header(&ann).unwrap();
        assert_eq!(h.msg_type, MSG_ANNOUNCE);
        // Body offsets: origin(10) utcOffset(2) reserved(1) → p1 at 34+13.
        assert_eq!(ann[47], 128); // priority1
        assert_eq!(ann[48], 0x06); // clockClass
        assert_eq!(ann[49], 0x21); // clockAccuracy
        assert_eq!(u16::from_be_bytes([ann[50], ann[51]]), 0x436A); // variance
        assert_eq!(ann[52], 128); // priority2
        assert_eq!(&ann[53..61], &id.to_be_bytes()); // grandmasterIdentity
        assert_eq!(u16::from_be_bytes([ann[61], ann[62]]), 0); // stepsRemoved
        assert_eq!(ann[63], 0x20); // timeSource = GPS
        assert_eq!(ann.len(), HEADER_LEN + 30);
    }

    #[test]
    fn delay_resp_echoes_seq_and_requesting_identity() {
        let req = build_delay_req(0x1111111111111111, 0x0042);
        let h = parse_header(&req).unwrap();
        let resp = build_delay_resp(0x2222222222222222, h.sequence_id, 1_500_000_000, &h.source_port_identity);
        let rh = parse_header(&resp).unwrap();
        assert_eq!(rh.msg_type, MSG_DELAY_RESP);
        assert_eq!(rh.sequence_id, 0x0042);
        // receiveTimestamp then requestingPortIdentity.
        assert_eq!(read_timestamp(&resp, HEADER_LEN).unwrap(), 1_500_000_000);
        assert_eq!(&resp[HEADER_LEN + 10..HEADER_LEN + 20], &h.source_port_identity);
    }

    #[test]
    fn timestamp_roundtrip() {
        let mut buf = vec![0u8; HEADER_LEN];
        let ns = 123_456_789_012_345u64;
        write_timestamp(&mut buf, ns);
        assert_eq!(read_timestamp(&buf, HEADER_LEN).unwrap(), ns);
    }

    #[test]
    fn master_clock_is_monotonic_from_zero() {
        let c = PtpMasterClock::new();
        let a = c.now_ns();
        std::thread::sleep(Duration::from_millis(5));
        let b = c.now_ns();
        assert!(b > a);
        // Session-relative, not wall-clock: well under an hour at start.
        assert!(a < 3_600_000_000_000);
    }

    #[test]
    fn uuid_is_canonical_36_chars() {
        let u = format_uuid([0xAB; 16]);
        assert_eq!(u.len(), 36);
        assert_eq!(u.matches('-').count(), 4);
    }
}
