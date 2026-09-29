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
use log::{debug, info, warn};
use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Convert a latency in milliseconds to sample frames at the wire rate —
/// the unit the sync-packet anchor and `Audio-Latency` speak. 2000 ms →
/// 88200, the iTunes constant.
pub fn latency_ms_to_samples(ms: u32) -> u32 {
    (ms as u64 * crate::WIRE_SAMPLE_RATE as u64 / 1000) as u32
}

/// NTP epoch offset — seconds between 1900-01-01 and 1970-01-01.
const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

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

/// A duration as a 32.32 NTP interval.
fn duration_to_ntp(d: Duration) -> u64 {
    (d.as_secs() << 32) | (((d.subsec_nanos() as u64) << 32) / 1_000_000_000)
}

/// Our NTP clock at instant `t` (past or future): [`ntp_now`] moved by
/// the monotonic distance to `t`.
pub fn ntp_at(t: Instant) -> u64 {
    ntp_at_from(t, Instant::now(), ntp_now())
}

/// [`ntp_at`] given a simultaneous reading of both clocks: `ntp` is our
/// NTP clock at `now`.
fn ntp_at_from(t: Instant, now: Instant, ntp: u64) -> u64 {
    if t >= now {
        ntp.wrapping_add(duration_to_ntp(t - now))
    } else {
        ntp.wrapping_sub(duration_to_ntp(now - t))
    }
}

/// Wall-clock length of one realtime packet (352 frames at the wire rate)
/// — the realtime sender's pacing step.
pub fn realtime_packet_duration() -> Duration {
    Duration::from_nanos((crate::airplay::rtp::FRAMES_PER_PACKET as u64 * 1_000_000_000) / crate::WIRE_SAMPLE_RATE as u64)
}

/// The realtime sender's schedule, shared with the session's sync sender:
/// the packet `n` after the start (rtptime `initial + 352·n`) is due at
/// `start + (n+1)·`[`realtime_packet_duration`]. The sender paces to it,
/// keeps it through re-glues, and publishes the next packet's (rtptime,
/// due instant) after every send; a sync packet built from that point
/// states the exact send time of the rtptime it names, whenever the sync
/// thread happens to wake — no 0-8 ms sampling error, and one value for
/// every member.
pub struct SendSchedule {
    start: Instant,
    next: Mutex<(u32, Instant)>,
}

impl SendSchedule {
    /// A schedule starting now, first packet `initial_rtptime`.
    pub fn new(initial_rtptime: u32) -> Arc<Self> {
        let start = Instant::now();
        Arc::new(Self { start, next: Mutex::new((initial_rtptime, start + realtime_packet_duration())) })
    }

    /// When the sender's packet clock started.
    pub fn start(&self) -> Instant {
        self.start
    }

    /// Due instant of the packet `packet_count` packets after the start.
    pub fn due(&self, packet_count: u64) -> Instant {
        self.start + realtime_packet_duration().saturating_mul((packet_count + 1) as u32)
    }

    /// The next packet to send: its rtptime and packet index.
    pub fn set_next(&self, rtptime: u32, packet_count: u64) {
        *self.next.lock().unwrap() = (rtptime, self.due(packet_count));
    }

    /// `(rtptime, due instant)` of the next packet to send.
    pub fn next(&self) -> (u32, Instant) {
        *self.next.lock().unwrap()
    }
}

/// The clock an AirPlay 2 session's sync packets are expressed on.
#[derive(Clone)]
pub enum SyncClock {
    /// 28-byte 0xD7 packets on the PTP timeline (the followed receiver's
    /// clock once locked, else ours).
    Ptp(crate::airplay::ap2_ptp::PtpTimeline),
    /// 20-byte 0xD4 packets on our NTP clock.
    Ntp,
}

/// Where a session's sync packets take their (rtptime, time) pair from.
#[derive(Clone)]
pub enum SyncTime {
    /// The rtptime the sender published last and the clock read when the
    /// sync thread wakes — what single receivers (and one-member
    /// pair/group sessions) use.
    Sampled(Arc<AtomicU32>),
    /// The sender's schedule: the next packet's rtptime and its exact due
    /// instant (pair/group sessions of two or more members).
    Scheduled(Arc<SendSchedule>),
}

/// One sync packet for a session: the same bytes for every member.
pub fn session_sync_packet(first: bool, latency: u32, clock: &SyncClock, time: &SyncTime) -> Vec<u8> {
    let (cur_rtp, due) = match time {
        SyncTime::Sampled(rtp) => (rtp.load(Ordering::Acquire), None),
        SyncTime::Scheduled(s) => {
            let (rtp, at) = s.next();
            (rtp, Some(at))
        }
    };
    match clock {
        SyncClock::Ptp(timeline) => {
            let (clock_id, ns) = timeline.time_at(due.unwrap_or_else(Instant::now));
            build_ptp_sync(first, cur_rtp, latency, ns, clock_id).to_vec()
        }
        SyncClock::Ntp => {
            let ntp = due.map(ntp_at).unwrap_or_else(ntp_now);
            build_ntp_sync(first, cur_rtp, latency, ntp).to_vec()
        }
    }
}

/// One sync sender for a whole AirPlay 2 session: every tick builds ONE
/// packet ([`session_sync_packet`]) and sends it to every member's control
/// address from that member's control socket. Per-member threads sampled
/// the rtptime→time mapping independently, so the halves of a pair were
/// told mappings up to a packet (8 ms) apart. The initial (0x90) packet is
/// sent to every member before this returns (anchor before audio).
pub fn spawn_session_sync_sender(
    dests: Vec<(UdpSocket, SocketAddr)>,
    latency_samples: u32,
    clock: SyncClock,
    time: SyncTime,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
) -> std::io::Result<thread::JoinHandle<()>> {
    let kind = match clock {
        SyncClock::Ptp(_) => "PTP",
        SyncClock::Ntp => "NTP",
    };
    spawn_sync_sender_inner(dests, stop_flag, receiver_name, kind, move |first| {
        session_sync_packet(first, latency_samples, &clock, &time)
    })
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
            let mut served: u64 = 0;
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
                        served += 1;
                        if served == 1 {
                            info!(
                                "AirPlay NTP timing: receiver engaged (first timing request from {})",
                                peer
                            );
                        }
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
    // Echo the request's sequence field (libraop copies the whole
    // header). Sonos always sends the constant 0x0007 so this is
    // byte-identical there, but receivers that increment the timing
    // seq need the echo to pair request and reply.
    resp[2..4].copy_from_slice(&req[2..4]);
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

/// Sleep for `total` in 100 ms slices, returning `false` early the moment
/// `stop_flag` is set (so shutdown latency stays bounded regardless of the
/// caller's cadence). Returns `true` when the full duration elapsed.
pub fn sleep_unless_stopped(stop_flag: &AtomicBool, total: Duration) -> bool {
    let slices = (total.as_millis() / 100).max(1);
    for _ in 0..slices {
        if stop_flag.load(Ordering::Acquire) {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    !stop_flag.load(Ordering::Acquire)
}

/// Spawn the sync packet sender. Sends one 20-byte sync packet per
/// second to the receiver's `control_port` until `stop_flag` is set.
///
/// Latency is the receiver-advertised buffer depth in samples
/// (typically 11025 = 250 ms at 44.1 kHz) — we use it to compute the
/// "now should be playing" anchor timestamp.
///
/// The initial (`0x90` extension-bit) sync is sent **synchronously,
/// before this function returns** — every field-proven sender strictly
/// orders the first sync before the first audio packet (iTunes: 254 µs
/// before; libraop sends it under the same lock as the first chunk), so
/// callers get anchor-before-audio by construction as long as they call
/// this before spawning their audio sender.
pub fn spawn_sync_sender(
    control_socket: UdpSocket,
    receiver_addr: SocketAddr,
    current_rtptime: Arc<AtomicU32>,
    latency_samples: u32,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
) -> std::io::Result<thread::JoinHandle<()>> {
    // NTP path: classic 20-byte 0xD4 sync, time field = our NTP clock.
    spawn_sync_sender_inner(
        vec![(control_socket, receiver_addr)],
        stop_flag,
        receiver_name,
        "NTP",
        move |first| {
            build_ntp_sync(first, current_rtptime.load(Ordering::Acquire), latency_samples, ntp_now()).to_vec()
        },
    )
}

fn spawn_sync_sender_inner<F>(
    dests: Vec<(UdpSocket, SocketAddr)>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
    kind: &'static str,
    build_packet: F,
) -> std::io::Result<thread::JoinHandle<()>>
where
    F: Fn(bool) -> Vec<u8> + Send + 'static,
{
    // Anchor first: the initial (extension-bit) sync goes out on the
    // caller's thread, before this function returns — so "first sync
    // precedes first audio" holds by construction when the caller spawns
    // its audio sender afterwards, with no flag to keep consistent.
    let first_pkt = build_packet(true);
    for (socket, addr) in &dests {
        match socket.send_to(&first_pkt, addr) {
            Ok(_) => info!(
                "AirPlay {} sync: anchor sync sent to {} ({} bytes); continuing at 1 Hz",
                kind,
                addr,
                first_pkt.len()
            ),
            Err(e) => warn!("AirPlay {} sync: initial anchor send to {} failed: {}", kind, addr, e),
        }
    }

    thread::Builder::new()
        .name(format!("stream-to-speaker-airplay-sync:{}", receiver_name))
        .spawn(move || {
            let mut count: u64 = 1;
            loop {
                // ~1 s cadence, stop-aware.
                if !sleep_unless_stopped(&stop_flag, Duration::from_secs(1)) {
                    break;
                }
                // One packet per tick, the same bytes to every destination.
                let pkt = build_packet(false);
                for (socket, addr) in &dests {
                    if let Err(e) = socket.send_to(&pkt, addr) {
                        warn!("AirPlay {} sync send to {} failed: {}", kind, addr, e);
                    }
                }
                count += 1;
            }
            debug!("AirPlay {} sync sender exiting after {} packets", kind, count);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_sync_packet_layout() {
        let p = build_ntp_sync(true, 100_000, 11025, 0xAABBCCDD11223344);
        assert_eq!(p.len(), 20);
        assert_eq!(p[0], 0x90); // first → marker
        assert_eq!(p[1], 0xD4);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 0x0007);
        assert_eq!(u32::from_be_bytes([p[4], p[5], p[6], p[7]]), 100_000 - 11025);
        assert_eq!(u64::from_be_bytes(p[8..16].try_into().unwrap()), 0xAABBCCDD11223344);
        assert_eq!(u32::from_be_bytes([p[16], p[17], p[18], p[19]]), 100_000);
        assert_eq!(build_ntp_sync(false, 1, 0, 0)[0], 0x80); // subsequent → no marker
    }

    #[test]
    fn ptp_sync_packet_is_owntone_0xd7_form() {
        let p = build_ptp_sync(true, 100_000, 11025, 0x0123456789, 0xDEADBEEFCAFEF00D);
        assert_eq!(p.len(), 28); // RTCP_SYNC_PACKET_PTP_LEN
        assert_eq!(p[0], 0x90);
        assert_eq!(p[1], 0xD7); // PT 215, NOT 0xD4
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 0x0006);
        assert_eq!(u32::from_be_bytes([p[4], p[5], p[6], p[7]]), 100_000 - 11025);
        // Raw nanoseconds — no NTP epoch, no 32.32.
        assert_eq!(u64::from_be_bytes(p[8..16].try_into().unwrap()), 0x0123456789);
        assert_eq!(u32::from_be_bytes([p[16], p[17], p[18], p[19]]), 100_000);
        // Trailing clock identity.
        assert_eq!(u64::from_be_bytes(p[20..28].try_into().unwrap()), 0xDEADBEEFCAFEF00D);
    }

    #[test]
    fn scheduled_sync_time_is_the_due_time_of_the_rtptime_it_names() {
        let schedule = SendSchedule::new(1000);
        let dur = realtime_packet_duration();
        assert_eq!(schedule.next(), (1000, schedule.start() + dur));
        // After 10 packets (and after a re-glue, which only moves the
        // count), the next rtptime is due exactly on the schedule.
        schedule.set_next(1000 + 352 * 10, 10);
        assert_eq!(schedule.next(), (1000 + 3520, schedule.start() + dur * 11));
        assert_eq!(schedule.due(0), schedule.start() + dur);

        // NTP: the time field is the due instant, whenever the packet is
        // built — two builds 100 ms apart carry the same mapping (within
        // the wall clock's own read jitter; a sampled time would be 100 ms
        // later).
        let time = SyncTime::Scheduled(schedule.clone());
        let a = session_sync_packet(false, 11025, &SyncClock::Ntp, &time);
        std::thread::sleep(Duration::from_millis(100));
        let b = session_sync_packet(false, 11025, &SyncClock::Ntp, &time);
        assert_eq!(&a[..8], &b[..8], "same rtptimes");
        assert_eq!(&a[16..20], &b[16..20]);
        let ta = u64::from_be_bytes(a[8..16].try_into().unwrap());
        let tb = u64::from_be_bytes(b[8..16].try_into().unwrap());
        let ms_apart = (ta.abs_diff(tb) as u128 * 1000) >> 32;
        assert!(ms_apart < 50, "{ms_apart} ms apart");
    }

    #[test]
    fn ntp_at_moves_ntp_now_by_the_monotonic_distance() {
        let now = Instant::now();
        let ntp = 1000u64 << 32;
        assert_eq!(ntp_at_from(now, now, ntp), ntp);
        assert_eq!(ntp_at_from(now + Duration::from_millis(1500), now, ntp), ntp + ((1 << 32) | (1 << 31)));
        assert_eq!(ntp_at_from(now, now + Duration::from_secs(3), ntp), ntp - (3 << 32));
        assert_eq!(duration_to_ntp(Duration::from_millis(1500)), (1 << 32) | (1 << 31));
        // The live version reads both clocks: 3 s ahead is ≈ 3 s later.
        let secs = ntp_at(now + Duration::from_secs(3)).wrapping_sub(ntp_at(now)) as f64 / 4_294_967_296.0;
        assert!((secs - 3.0).abs() < 0.1, "{secs}");
    }

    #[test]
    fn one_session_sync_sender_sends_every_member_the_same_packet() {
        let rx_a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let rx_b = UdpSocket::bind("127.0.0.1:0").unwrap();
        for rx in [&rx_a, &rx_b] {
            rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        }
        let dests = vec![
            (UdpSocket::bind("127.0.0.1:0").unwrap(), rx_a.local_addr().unwrap()),
            (UdpSocket::bind("127.0.0.1:0").unwrap(), rx_b.local_addr().unwrap()),
        ];
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_session_sync_sender(
            dests,
            11025,
            SyncClock::Ntp,
            SyncTime::Scheduled(SendSchedule::new(5000)),
            stop.clone(),
            "test".into(),
        )
        .unwrap();
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        let na = rx_a.recv(&mut a).unwrap();
        let nb = rx_b.recv(&mut b).unwrap();
        stop.store(true, Ordering::Release);
        handle.join().unwrap();
        assert_eq!(na, 20);
        assert_eq!(a[..na], b[..nb], "byte-identical anchor sync to both members");
        assert_eq!(a[0], 0x90, "the first packet is the anchor sync");
        assert_eq!(u32::from_be_bytes([a[16], a[17], a[18], a[19]]), 5000);
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

/// 20-byte NTP audio sync packet (PT 0xD4). Anchor: the RTP timestamp that
/// should be audible (current − latency) at the carried NTP time, then the
/// current write head. Matches the classic RAOP/OwnTone NTP sync.
fn build_ntp_sync(first: bool, cur_rtp: u32, latency: u32, ntp_time: u64) -> [u8; 20] {
    let mut pkt = [0u8; 20];
    pkt[0] = if first { 0x90 } else { 0x80 };
    pkt[1] = 0xD4;
    BigEndian::write_u16(&mut pkt[2..4], 0x0007);
    BigEndian::write_u32(&mut pkt[4..8], cur_rtp.wrapping_sub(latency));
    BigEndian::write_u64(&mut pkt[8..16], ntp_time);
    BigEndian::write_u32(&mut pkt[16..20], cur_rtp);
    pkt
}

/// 28-byte PTP audio sync packet (PT 0xD7) — OwnTone's `sync_packet_ptp_make`.
/// Differs from the NTP packet in three load-bearing ways: type 0xD7 (not
/// 0xD4), the time field is the **raw monotonic clock value (ns)** we serve
/// as PTP grandmaster (no NTP epoch, no 32.32 fixed-point), and the trailing
/// 8 bytes carry our clock identity so the receiver can map RTP → our clock.
fn build_ptp_sync(first: bool, cur_rtp: u32, latency: u32, ptp_ns: u64, clock_id: u64) -> [u8; 28] {
    let mut pkt = [0u8; 28];
    pkt[0] = if first { 0x90 } else { 0x80 };
    pkt[1] = 0xD7;
    BigEndian::write_u16(&mut pkt[2..4], 0x0006);
    BigEndian::write_u32(&mut pkt[4..8], cur_rtp.wrapping_sub(latency));
    BigEndian::write_u64(&mut pkt[8..16], ptp_ns);
    BigEndian::write_u32(&mut pkt[16..20], cur_rtp);
    BigEndian::write_u64(&mut pkt[20..28], clock_id);
    pkt
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
/// Retransmission counters for one session, shared between the resend
/// responder (writer) and whoever reports them (the Stats card, the log
/// line at exit). Resend requests are the receiver telling us packets
/// went missing: a rising rate at a low AirPlay buffer means the network
/// can't sustain that buffer — the one signal a user needs to decide to
/// raise it.
#[derive(Default, Debug)]
pub struct ResendStats {
    /// Resend requests received (each names a run of sequence numbers).
    pub requests: AtomicU64,
    /// Packets actually re-sent in response.
    pub packets: AtomicU64,
}

impl ResendStats {
    /// `(requests, packets re-sent)`.
    pub fn snapshot(&self) -> (u64, u64) {
        (self.requests.load(Ordering::Relaxed), self.packets.load(Ordering::Relaxed))
    }
}

pub fn spawn_resend_responder(
    control_socket: UdpSocket,
    receiver_control_addr: SocketAddr,
    resend: Arc<ResendBuffer>,
    stats: Arc<ResendStats>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
) -> std::io::Result<thread::JoinHandle<()>> {
    control_socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    thread::Builder::new()
        .name(format!("stream-to-speaker-airplay-resend:{}", receiver_name))
        .spawn(move || {
            let mut buf = [0u8; 64];
            let mut requests: u64 = 0;
            while !stop_flag.load(Ordering::Acquire) {
                match control_socket.recv_from(&mut buf) {
                    // Resend request: 0x80 0xD5, seq(2), first(2), count(2).
                    Ok((n, peer)) if n >= 8 && buf[1] == 0xD5 => {
                        requests += 1;
                        stats.requests.fetch_add(1, Ordering::Relaxed);
                        if requests == 1 {
                            // A resend request proves the receiver is
                            // consuming our RTP stream (it tracks seq gaps).
                            info!(
                                "AirPlay resend: receiver {} is consuming the RTP stream (first resend request)",
                                peer
                            );
                        }
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
                        stats.packets.fetch_add(sent as u64, Ordering::Relaxed);
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
            let (req, pk) = stats.snapshot();
            info!(
                "AirPlay resend responder: exiting after {} resend request(s), {} packet(s) re-sent",
                req, pk
            );
        })
}
