//! IEEE-1588 (PTPv2) timing for AirPlay 2 — the path HomePods expect.
//!
//! AirPlay 2 receivers that advertise `SupportsPTP` (feature bit 41)
//! synchronise playback over PTP rather than the legacy NTP timing
//! packets. The model (per OwnTone / `airplay2-rs` / the AirPlay 2
//! reverse-engineering): the sender briefly advertises itself as a PTP
//! master, then **yields** (BMCA) to the HomePod, which becomes the
//! grandmaster; the sender then runs as a **follower**, measuring the
//! offset between its local clock and the HomePod's so it can stamp audio
//! against the shared timeline.
//!
//! We implement a pragmatic unicast follower: we exchange Sync/Follow_Up
//! (receiver→us) and Delay_Req/Delay_Resp (us→receiver) on UDP ports
//! 319 (event) and 320 (general) and maintain a smoothed clock offset.
//!
//! Offset sign convention (derived, not copied — easy to get backwards):
//! with `t1`=master Sync send, `t2`=our Sync recv, `t3`=our Delay_Req
//! send, `t4`=master Delay_Req recv,
//! ```text
//!   offset = ((t1 - t2) + (t4 - t3)) / 2     // = master − local
//!   master_time = local_time + offset
//! ```
//!
//! ## Status
//!
//! Unit-tested at the packet-codec + offset-math level. The live PTP
//! handshake has not been validated against a real HomePod; if a HomePod
//! rejects realtime+PTP it likely wants buffered/AAC audio (a separate
//! follow-up). On Windows binding 319/320 needs no special privilege; on
//! Unix it needs `CAP_NET_BIND_SERVICE` (we fall back to ephemeral ports,
//! which only matters for local testing).

use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PTP_EVENT_PORT: u16 = 319;
pub const PTP_GENERAL_PORT: u16 = 320;

const PTP_VERSION: u8 = 2;
const HEADER_LEN: usize = 34;

// Message types (low nibble of byte 0).
const MSG_SYNC: u8 = 0x0;
const MSG_DELAY_REQ: u8 = 0x1;
const MSG_FOLLOW_UP: u8 = 0x8;
const MSG_DELAY_RESP: u8 = 0x9;
const MSG_ANNOUNCE: u8 = 0xB;

/// Our advertised priority1. HomePods advertise 248; we start at 250 so
/// the lower (HomePod) wins BMCA and we yield to it.
const OUR_PRIORITY1: u8 = 250;

/// Shared, smoothed clock offset (master − local) in nanoseconds.
pub struct PtpClock {
    offset_ns: AtomicI64,
    synced: AtomicBool,
}

impl PtpClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            offset_ns: AtomicI64::new(0),
            synced: AtomicBool::new(false),
        })
    }

    /// Current estimate of the HomePod's PTP time, in ns since the Unix
    /// epoch (the PTP timescale base we use locally). Before sync, this is
    /// just our local clock.
    pub fn master_now_ns(&self) -> i64 {
        local_now_ns() + self.offset_ns.load(Ordering::Acquire)
    }

    pub fn is_synchronized(&self) -> bool {
        self.synced.load(Ordering::Acquire)
    }

    fn update(&self, offset_ns: i64) {
        // Light EMA smoothing to ride out jitter once locked.
        let prev = self.offset_ns.load(Ordering::Acquire);
        let next = if self.synced.load(Ordering::Acquire) {
            (prev * 7 + offset_ns) / 8
        } else {
            offset_ns
        };
        self.offset_ns.store(next, Ordering::Release);
        self.synced.store(true, Ordering::Release);
    }
}

/// Handle to a running PTP follower.
pub struct PtpSession {
    pub clock: Arc<PtpClock>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PtpSession {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PtpSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Start the PTP follower against `receiver_ip`. Binds the event (319)
/// and general (320) ports, falling back to ephemeral ports if the
/// privileged bind is refused (Unix dev only — won't actually sync there).
pub fn spawn_ptp(receiver_ip: IpAddr, local_ip: IpAddr, receiver_name: String) -> Result<PtpSession> {
    let event = bind_ptp(local_ip, PTP_EVENT_PORT).context("bind PTP event :319")?;
    let general = bind_ptp(local_ip, PTP_GENERAL_PORT).context("bind PTP general :320")?;
    event.set_read_timeout(Some(Duration::from_millis(200)))?;
    general.set_read_timeout(Some(Duration::from_millis(200)))?;

    let clock = PtpClock::new();
    let stop = Arc::new(AtomicBool::new(false));
    let clock_id = clock_identity_from_ip(local_ip);

    let clock_t = clock.clone();
    let stop_t = stop.clone();
    let handle = thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-ptp:{}", receiver_name))
        .spawn(move || {
            run_follower(event, general, receiver_ip, clock_id, clock_t, stop_t);
        })?;

    Ok(PtpSession { clock, stop, handle: Some(handle) })
}

fn bind_ptp(local_ip: IpAddr, port: u16) -> Result<UdpSocket> {
    match UdpSocket::bind(SocketAddr::new(local_ip, port)) {
        Ok(s) => Ok(s),
        Err(e) => {
            warn!("PTP: privileged bind {} failed ({}); using ephemeral (won't sync on this OS)", port, e);
            UdpSocket::bind(SocketAddr::new(local_ip, 0)).map_err(Into::into)
        }
    }
}

fn run_follower(
    event: UdpSocket,
    general: UdpSocket,
    receiver_ip: IpAddr,
    clock_id: [u8; 8],
    clock: Arc<PtpClock>,
    stop: Arc<AtomicBool>,
) {
    let event_dst = SocketAddr::new(receiver_ip, PTP_EVENT_PORT);
    let general_dst = SocketAddr::new(receiver_ip, PTP_GENERAL_PORT);

    // Briefly announce ourselves (priority1=250) so the receiver runs
    // BMCA and elects itself grandmaster, then we become a pure follower.
    for seq in 0..3u16 {
        let ann = build_announce(&clock_id, seq, OUR_PRIORITY1);
        let _ = general.send_to(&ann, general_dst);
        thread::sleep(Duration::from_millis(100));
        if stop.load(Ordering::Acquire) {
            return;
        }
    }
    info!("AirPlay 2 PTP: yielding to {} as grandmaster, following", receiver_ip);

    let mut dreq_seq: u16 = 0;
    // Pending Sync timing keyed by the master's sequenceId.
    let mut t1: Option<i64> = None; // master Sync origin (from Follow_Up)
    let mut t2: Option<i64> = None; // our Sync receive
    let mut t3: Option<i64> = None; // our Delay_Req send
    let mut sync_seq: u16 = 0;
    let mut buf = [0u8; 256];

    while !stop.load(Ordering::Acquire) {
        // Event port: Sync arrives here.
        if let Ok((n, _)) = event.recv_from(&mut buf) {
            let now = local_now_ns();
            if let Some(h) = parse_header(&buf[..n]) {
                if h.msg_type == MSG_SYNC {
                    t2 = Some(now);
                    sync_seq = h.sequence_id;
                    // Two-step: t1 comes in the Follow_Up. If the Sync was
                    // one-step (carries origin in itself), use it directly.
                    if h.two_step {
                        // wait for Follow_Up
                    } else if let Some(ts) = read_timestamp(&buf[..n], HEADER_LEN) {
                        t1 = Some(ts);
                    }
                    // Fire a Delay_Req to measure the reverse path.
                    let dreq = build_delay_req(&clock_id, dreq_seq);
                    if event.send_to(&dreq, event_dst).is_ok() {
                        t3 = Some(local_now_ns());
                    }
                    dreq_seq = dreq_seq.wrapping_add(1);
                }
            }
        }

        // General port: Follow_Up, Delay_Resp, Announce.
        if let Ok((n, _)) = general.recv_from(&mut buf) {
            if let Some(h) = parse_header(&buf[..n]) {
                match h.msg_type {
                    MSG_FOLLOW_UP if h.sequence_id == sync_seq => {
                        t1 = read_timestamp(&buf[..n], HEADER_LEN);
                    }
                    MSG_DELAY_RESP => {
                        // Delay_Resp: receiveTimestamp(10) then
                        // requestingPortIdentity(10). t4 = receive time.
                        let t4 = read_timestamp(&buf[..n], HEADER_LEN);
                        if let (Some(t1v), Some(t2v), Some(t3v), Some(t4v)) = (t1, t2, t3, t4) {
                            // offset = ((t1 - t2) + (t4 - t3)) / 2  (master − local)
                            let offset = ((t1v - t2v) + (t4v - t3v)) / 2;
                            clock.update(offset);
                            debug!("AP2 PTP offset = {} ns (synced={})", offset, clock.is_synchronized());
                            t1 = None;
                            t2 = None;
                            t3 = None;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    debug!("AirPlay 2 PTP follower exiting");
}

// ---------------------------------------------------------------------------
// Packet codec
// ---------------------------------------------------------------------------

struct PtpHeader {
    msg_type: u8,
    sequence_id: u16,
    two_step: bool,
}

fn parse_header(buf: &[u8]) -> Option<PtpHeader> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let msg_type = buf[0] & 0x0f;
    if buf[1] & 0x0f != PTP_VERSION {
        return None;
    }
    let flags = u16::from_be_bytes([buf[6], buf[7]]);
    let two_step = flags & 0x0200 != 0; // twoStepFlag
    let sequence_id = u16::from_be_bytes([buf[30], buf[31]]);
    Some(PtpHeader { msg_type, sequence_id, two_step })
}

fn build_header(out: &mut Vec<u8>, msg_type: u8, clock_id: &[u8; 8], seq: u16, body_len: usize, control: u8, log_interval: i8) {
    let total = HEADER_LEN + body_len;
    out.push((msg_type) & 0x0f); // transportSpecific=0
    out.push(PTP_VERSION & 0x0f);
    out.extend_from_slice(&(total as u16).to_be_bytes()); // messageLength
    out.push(0); // domainNumber
    out.push(0); // reserved
    out.extend_from_slice(&0u16.to_be_bytes()); // flags
    out.extend_from_slice(&0i64.to_be_bytes()); // correctionField
    out.extend_from_slice(&0u32.to_be_bytes()); // reserved
    out.extend_from_slice(clock_id); // clockIdentity (8)
    out.extend_from_slice(&1u16.to_be_bytes()); // sourcePortNumber
    out.extend_from_slice(&seq.to_be_bytes()); // sequenceId
    out.push(control); // controlField
    out.push(log_interval as u8); // logMessageInterval
}

fn build_delay_req(clock_id: &[u8; 8], seq: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 10);
    build_header(&mut out, MSG_DELAY_REQ, clock_id, seq, 10, 0x01, 0x7f);
    out.extend_from_slice(&[0u8; 10]); // originTimestamp (zero; t3 measured locally)
    out
}

fn build_announce(clock_id: &[u8; 8], seq: u16, priority1: u8) -> Vec<u8> {
    // Announce body: originTimestamp(10) currentUtcOffset(2) reserved(1)
    // grandmasterPriority1(1) grandmasterClockQuality(4)
    // grandmasterPriority2(1) grandmasterIdentity(8) stepsRemoved(2)
    // timeSource(1) = 30 bytes.
    let mut out = Vec::with_capacity(HEADER_LEN + 30);
    build_header(&mut out, MSG_ANNOUNCE, clock_id, seq, 30, 0x05, 1);
    out.extend_from_slice(&[0u8; 10]); // originTimestamp
    out.extend_from_slice(&0i16.to_be_bytes()); // currentUtcOffset
    out.push(0); // reserved
    out.push(priority1);
    // clockQuality: clockClass(1)=248 clockAccuracy(1)=0xFE offsetScaledLogVariance(2)
    out.push(248);
    out.push(0xFE);
    out.extend_from_slice(&0xFFFFu16.to_be_bytes());
    out.push(128); // priority2
    out.extend_from_slice(clock_id); // grandmasterIdentity
    out.extend_from_slice(&0u16.to_be_bytes()); // stepsRemoved
    out.push(0xA0); // timeSource = INTERNAL_OSCILLATOR
    out
}

/// Read a 10-byte PTP timestamp (48-bit seconds BE + 32-bit ns BE) at
/// `off` and return ns since epoch.
fn read_timestamp(buf: &[u8], off: usize) -> Option<i64> {
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
    Some(secs as i64 * 1_000_000_000 + nanos as i64)
}

#[allow(dead_code)]
fn write_timestamp(out: &mut Vec<u8>, ns: i64) {
    let secs = (ns / 1_000_000_000) as u64;
    let nanos = (ns % 1_000_000_000) as u32;
    out.push((secs >> 40) as u8);
    out.push((secs >> 32) as u8);
    out.push((secs >> 24) as u8);
    out.push((secs >> 16) as u8);
    out.push((secs >> 8) as u8);
    out.push(secs as u8);
    out.extend_from_slice(&nanos.to_be_bytes());
}

/// Local clock in ns since the Unix epoch (our PTP timescale base).
fn local_now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Derive an 8-byte PTP clockIdentity from the local IPv4 (EUI-64-ish:
/// not a real MAC, but stable per host and unique enough for one peer).
fn clock_identity_from_ip(ip: IpAddr) -> [u8; 8] {
    let mut id = [0u8; 8];
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            id = [o[0], o[1], 0xFF, 0xFE, o[2], o[3], 0x00, 0x01];
        }
        IpAddr::V6(v6) => {
            let o = v6.octets();
            id.copy_from_slice(&o[8..16]);
        }
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips() {
        let id = [1, 2, 3, 4, 5, 6, 7, 8];
        let dreq = build_delay_req(&id, 0x1234);
        let h = parse_header(&dreq).unwrap();
        assert_eq!(h.msg_type, MSG_DELAY_REQ);
        assert_eq!(h.sequence_id, 0x1234);
        // messageLength field reflects header + 10-byte body.
        assert_eq!(u16::from_be_bytes([dreq[2], dreq[3]]), (HEADER_LEN + 10) as u16);
        assert_eq!(dreq[1] & 0x0f, PTP_VERSION);
    }

    #[test]
    fn announce_carries_priority1_and_type() {
        let id = [0u8; 8];
        let ann = build_announce(&id, 7, 250);
        let h = parse_header(&ann).unwrap();
        assert_eq!(h.msg_type, MSG_ANNOUNCE);
        // priority1 sits after header(34) + originTimestamp(10) +
        // currentUtcOffset(2) + reserved(1).
        assert_eq!(ann[34 + 10 + 2 + 1], 250);
    }

    #[test]
    fn timestamp_roundtrip() {
        let mut buf = Vec::new();
        // header padding so the timestamp is at HEADER_LEN.
        buf.resize(HEADER_LEN, 0);
        let ns = 1_700_000_123_456_789_000i64;
        write_timestamp(&mut buf, ns);
        let got = read_timestamp(&buf, HEADER_LEN).unwrap();
        assert_eq!(got, ns);
    }

    #[test]
    fn offset_sign_master_minus_local() {
        // Master clock 5 ms ahead, symmetric 1 ms path delay:
        //   t1 = master send, t2 = t1 + d - offset(local-master)... build
        // directly from the convention: master = local + offset.
        // Suppose true offset (master-local) = +5_000_000 ns, delay 1ms.
        let offset_true = 5_000_000i64;
        let delay = 1_000_000i64;
        // local timeline points:
        let t2 = 100_000_000i64; // we receive Sync at local 100ms
        // master sent it `delay` earlier in master time:
        let t1 = (t2 + offset_true) - delay;
        let t3 = 110_000_000i64; // we send Delay_Req at local 110ms
        // master receives it `delay` later in master time:
        let t4 = (t3 + offset_true) + delay;
        let offset = ((t1 - t2) + (t4 - t3)) / 2;
        assert_eq!(offset, offset_true);
    }

    #[test]
    fn clock_master_now_applies_offset() {
        let c = PtpClock::new();
        assert!(!c.is_synchronized());
        c.update(1_000_000_000);
        assert!(c.is_synchronized());
        let m = c.master_now_ns();
        let l = local_now_ns();
        // master ≈ local + 1s (allow scheduling slack).
        assert!((m - l - 1_000_000_000).abs() < 50_000_000);
    }
}
