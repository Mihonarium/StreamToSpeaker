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
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const PTP_EVENT_PORT: u16 = 319;
pub const PTP_GENERAL_PORT: u16 = 320;

const PTP_VERSION: u8 = 2;
/// High nibble of header byte 0 — gPTP framing (see [`build_header`]),
/// used by pair/group sessions.
const TRANSPORT_SPECIFIC_GPTP: u8 = 0x10;
/// High nibble of header byte 0 — plain IEEE-1588 framing, what single
/// receivers have always been sent.
const TRANSPORT_SPECIFIC_PLAIN: u8 = 0x00;
const HEADER_LEN: usize = 34;

// Message types (low nibble of byte 0).
const MSG_SYNC: u8 = 0x0;
const MSG_DELAY_REQ: u8 = 0x1;
const MSG_FOLLOW_UP: u8 = 0x8;
const MSG_DELAY_RESP: u8 = 0x9;
const MSG_ANNOUNCE: u8 = 0xB;
const MSG_SIGNALING: u8 = 0xC;

// Header flags (big-endian u16 at bytes 6..8).
const FLAG_TWO_STEP: u16 = 1 << 9;
const FLAG_UNICAST: u16 = 1 << 10;
const FLAG_TIMESCALE: u16 = 1 << 3;
const FLAGS_GENERAL: u16 = FLAG_UNICAST | FLAG_TIMESCALE; // 0x0408
const FLAGS_SYNC: u16 = FLAGS_GENERAL | FLAG_TWO_STEP; // 0x0608

// logMessageInterval per message kind — libairptp's AIRPTP_LOGMESSAGEINT_*.
const LOG_INTERVAL_ANNOUNCE: i8 = 0; // 1 s
const LOG_INTERVAL_SYNC: i8 = -3; // 125 ms
const LOG_INTERVAL_DELAY_RESP: i8 = -3;
const LOG_INTERVAL_SIGNALING: i8 = -128;

// sourcePortIdentity port number — Apple stacks use 0x8005 (libairptp
// hardcodes it); receivers key on it being stable, not on the value.
const PORT_NUMBER: [u8; 2] = [0x80, 0x05];

// TLV machinery (IEEE-1588 §14): tlvType(2 BE) + lengthField(2 BE) + value.
const TLV_ORG_EXTENSION: u16 = 0x0003;
const TLV_PATH_TRACE: u16 = 0x0008;
/// Apple's organizationId for the proprietary AirPlay PTP signaling TLVs.
const ORG_APPLE: [u8; 3] = [0x00, 0x0d, 0x93];
/// Fixed leading bytes of both Apple signaling TLV payloads (libairptp's
/// `apple_unknown` — meaning unknown, value captured from iOS senders).
const APPLE_UNKNOWN: [u8; 4] = [0x00, 0x00, 0x03, 0x01];

// libairptp's AIRPTP_INTERVAL_MS_*.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_INTERVAL: Duration = Duration::from_millis(125);
const SIGNALING_INTERVAL: Duration = Duration::from_secs(1);
/// Follow mode: Delay_Req cadence towards the followed member.
const DELAY_REQ_INTERVAL: Duration = Duration::from_secs(1);
/// Follow mode: a member is followed only once it has announced its
/// grandmaster (the id must name the timeline its Syncs are on). One that
/// Syncs to us this long without ever announcing is followed on its Sync
/// source identity instead.
const FOLLOW_ANNOUNCE_WAIT: Duration = Duration::from_secs(3);
/// Follow mode: the followed member is dropped when no Sync/Follow_Up pair
/// from it completed for this long; the next member whose pair arrives is
/// followed instead, and its last clock stays in use until then.
const FOLLOW_STALE: Duration = Duration::from_secs(3);
/// Largest difference between one offset sample of the followed clock and
/// the smoothed offset that is still blended in. A different grandmaster
/// or a stepped clock moves the offset by seconds to days; network delay
/// jitter stays far below this. Under the pair/group rules a sample beyond
/// it is held until a second one agrees (a real step, relocked on at once)
/// and a lone outlier is dropped; a single receiver blends every sample.
const PEER_STEP_NS: u64 = 50_000_000;

/// The monotonic timeline we serve as PTP grandmaster. Receivers follow
/// this clock; the `0xD4` audio sync packets must be stamped from the
/// same instance (see [`crate::airplay::timing::session_sync_packet`]).
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

    /// Our PTP time at instant `t` (past or future; 0 before the clock
    /// started).
    pub fn ns_at(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.start).as_nanos() as u64
    }
}

/// The followed receiver's clock: the grandmaster it announces and the
/// offset of that clock from ours, published together (see
/// [`PeerClock::published`]).
///
/// A single receiver (`pair_rules` false) keeps the rules it has always
/// had: an Announce naming another grandmaster renames the lock in place
/// (the offset carries over and converges sample by sample), and every
/// sample is blended into the smoothed offset.
///
/// A pair/group session (`pair_rules` true) never publishes an id with an
/// offset taken on another timeline. After an Announce naming another
/// grandmaster the previous `(id, offset)` stays published until the next
/// Sync/Follow_Up pair, whose raw sample relocks on the new id. A step of
/// the offset (a grandmaster change whose Announce is still on its way, or
/// a stepped clock) is taken raw once a second sample confirms it; a lone
/// outlier is dropped instead of blended.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PeerClock {
    /// The pair/group session rules (see above).
    pair_rules: bool,
    /// Grandmaster identity the followed receiver announced last (0 = none
    /// yet).
    announced: u64,
    /// `(id, smoothed receiver − our offset in ns)` once locked. Samples
    /// taken before the first Announce lock under id 0, which is not
    /// published until that Announce names their clock. Under the pair
    /// rules the id lags `announced` until the next sample relocks.
    lock: Option<(u64, i64)>,
    /// Pair rules: a sample more than [`PEER_STEP_NS`] off the lock,
    /// waiting for a second one that agrees with it.
    step: Option<i64>,
    /// Bumped on every (re)lock and never reset: an anchor made on one
    /// epoch is stale on another (another grandmaster, a step), while the
    /// slow drift a smoothed offset follows keeps the epoch.
    epoch: u64,
}

/// What one offset sample did to the followed clock (for the log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerSample {
    /// First sample after a start, or (pair rules) after an announced
    /// grandmaster change: the raw sample is the offset.
    Locked(i64),
    /// Blended into the smoothed offset.
    Tracked(i64),
    /// Pair rules: too far from the smoothed offset; held until a second
    /// sample confirms a step (a lone outlier is dropped).
    Held(i64),
    /// Pair rules: a step confirmed by two samples, relocked on the raw
    /// sample.
    Stepped(i64),
}

impl PeerSample {
    /// The offset the sample produced (or, when held, the sample itself).
    fn offset(self) -> i64 {
        match self {
            PeerSample::Locked(o) | PeerSample::Tracked(o) | PeerSample::Held(o) | PeerSample::Stepped(o) => o,
        }
    }
}

impl PeerClock {
    fn new(pair_rules: bool) -> Self {
        Self { pair_rules, ..Self::default() }
    }

    /// The followed receiver announces grandmaster `id`. Returns the
    /// previously announced id.
    fn announce(&mut self, id: u64) -> u64 {
        let prev = self.announced;
        if id == prev {
            return prev;
        }
        self.announced = id;
        match self.lock {
            // Samples taken before the first Announce: it names their
            // clock. A single receiver also keeps its offset under a newly
            // announced id.
            Some((locked_id, off)) if locked_id == 0 || !self.pair_rules => self.lock = Some((id, off)),
            // Pair rules, another grandmaster: the previous clock stays
            // published until the next sample relocks on the new one.
            Some(_) => self.step = None,
            None => {}
        }
        prev
    }

    /// One Sync/Follow_Up offset sample (`t1 − t2`) of the followed clock.
    fn sample(&mut self, s: i64) -> PeerSample {
        match self.lock {
            None => self.relock(self.announced, s),
            // Pair rules: a grandmaster change was announced since the lock.
            Some((id, _)) if self.pair_rules && id != self.announced => self.relock(self.announced, s),
            Some((id, prev)) if !self.pair_rules || s.abs_diff(prev) <= PEER_STEP_NS => {
                let next = (prev / 8) * 7 + s / 8;
                self.lock = Some((id, next));
                self.step = None;
                PeerSample::Tracked(next)
            }
            Some(_) => match self.step {
                Some(held) if s.abs_diff(held) <= PEER_STEP_NS => {
                    self.relock(self.announced, s);
                    PeerSample::Stepped(s)
                }
                _ => {
                    self.step = Some(s);
                    PeerSample::Held(s)
                }
            },
        }
    }

    /// Lock on grandmaster `id` with the raw sample `s`: a new epoch.
    fn relock(&mut self, id: u64, s: i64) -> PeerSample {
        self.announced = id;
        self.lock = Some((id, s));
        self.step = None;
        self.epoch += 1;
        PeerSample::Locked(s)
    }

    /// `(id, offset)` readers may use: locked on an announced clock.
    fn published(&self) -> Option<(u64, i64)> {
        self.lock.filter(|(id, _)| *id != 0)
    }
}

/// Cloneable view of both timelines in play: **ours** (we serve it as a
/// PTP master) and **the receiver's** (it serves its own clock to us —
/// field-tested Sonos accepts SETRATEANCHORTIME *only* on its own
/// timeline, so the sender must follow the receiver's Sync/Follow_Up
/// stream to express "now" on it).
#[derive(Clone)]
pub struct PtpTimeline {
    pub clock: Arc<PtpMasterClock>,
    /// Our 8-byte clock identity as a u64 — goes BE into every PTP header
    /// and (as int64) into the RTSP `timingPeerInfo.ClockID`.
    pub clock_id: u64,
    /// The followed receiver's clock (id + offset, one lock so they are
    /// always read together).
    peer: Arc<Mutex<PeerClock>>,
}

impl PtpTimeline {
    /// `pair_rules`: the followed clock takes the pair/group session rules
    /// (see [`PeerClock`]).
    fn new(clock: Arc<PtpMasterClock>, clock_id: u64, pair_rules: bool) -> Self {
        Self { clock, clock_id, peer: Arc::new(Mutex::new(PeerClock::new(pair_rules))) }
    }

    pub fn our_now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    /// `(followed clock id, followed − our offset in ns)` once locked onto
    /// a receiver's announced clock; None before that.
    pub fn followed(&self) -> Option<(u64, i64)> {
        self.peer.lock().unwrap().published()
    }

    /// [`Self::followed`] plus the lock's epoch, which changes whenever the
    /// followed clock is (re)locked — another grandmaster or a step — but
    /// not with the slow drift the smoothed offset tracks.
    pub fn followed_epoch(&self) -> Option<(u64, i64, u64)> {
        let p = self.peer.lock().unwrap();
        p.published().map(|(id, off)| (id, off, p.epoch))
    }

    /// `(receiver clock id, receiver "now" in ns)` once we've locked onto
    /// the receiver's Sync/Follow_Up stream; None before that.
    pub fn receiver_now_ns(&self) -> Option<(u64, u64)> {
        let (id, off) = self.followed()?;
        on_followed_clock(self.clock.now_ns(), off).map(|n| (id, n))
    }

    /// `(clock id, time in ns)` of instant `t`: on the followed receiver's
    /// clock once locked, else on ours — the clock sync packets name.
    pub fn time_at(&self, t: Instant) -> (u64, u64) {
        let ours = self.clock.ns_at(t);
        match self.followed().and_then(|(id, off)| on_followed_clock(ours, off).map(|n| (id, n))) {
            Some(followed) => followed,
            None => (self.clock_id, ours),
        }
    }

    /// The followed receiver announced grandmaster `id`; returns the id
    /// it announced before (0 = none).
    fn note_announce(&self, id: u64) -> u64 {
        self.peer.lock().unwrap().announce(id)
    }

    /// Ingest one Sync/Follow_Up pair from the followed receiver: `t1` =
    /// its precise origin timestamp, `t2` = our receive time. One-way path
    /// delay (sub-ms on a LAN) is absorbed into the offset — irrelevant at
    /// audio-anchor precision.
    fn note_sample(&self, t1_ns: u64, t2_ns: u64) -> PeerSample {
        self.peer.lock().unwrap().sample(t1_ns as i64 - t2_ns as i64)
    }

    /// Follow mode: follow grandmaster `id` from this Sync/Follow_Up pair
    /// on, relocked on its raw sample. One step under the lock, so readers
    /// see the previous clock until the new one is in place.
    fn relock(&self, id: u64, t1_ns: u64, t2_ns: u64) -> PeerSample {
        self.peer.lock().unwrap().relock(id, t1_ns as i64 - t2_ns as i64)
    }
}

/// `our_ns` on the followed clock (`our + offset`); None if out of range.
fn on_followed_clock(our_ns: u64, offset: i64) -> Option<u64> {
    i64::try_from(our_ns).ok()?.checked_add(offset).and_then(|n| u64::try_from(n).ok())
}

/// Handle to a running PTP master.
pub struct PtpMaster {
    pub timeline: PtpTimeline,
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

/// How the PTP layer treats the receivers' own clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtpMode {
    /// We serve as grandmaster, and with exactly one receiver we also
    /// follow that receiver's own clock once it announces one and Syncs to
    /// us (field-tested on Sonos). What a single receiver runs; a
    /// one-member pair/group `master` session runs it too, with the pair
    /// rules ([`PtpOptions::pair_session`]).
    Single,
    /// Pair/group `master` with two or more members: we serve as
    /// grandmaster; the members' own clocks are logged, never followed, so
    /// anchors and sync packets are always on our clock.
    Master,
    /// Pair/group `follow`: we send no Announce, Sync or Signaling; the
    /// first member whose Sync/Follow_Up pair locks becomes the timeline
    /// anchors and sync packets are expressed on, and we send it Delay_Req.
    Follow,
}

/// Per-session PTP options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtpOptions {
    pub mode: PtpMode,
    /// Send gPTP framing (header transportSpecific = 1). Pair/group
    /// sessions use it; single receivers keep the plain-1588 byte unless
    /// the config forces it.
    pub gptp_framing: bool,
    /// A pair/group session. The followed clock takes the pair rules (see
    /// [`PeerClock`]), and in [`PtpMode::Single`] only the lone member's
    /// own Announce and Sync/Follow_Up count. False for a single receiver:
    /// any Announce and Sync/Follow_Up that reach us feed the followed
    /// clock, with the single-receiver rules.
    pub pair_session: bool,
}

impl PtpOptions {
    /// The single-receiver options: follow a lone receiver's clock, plain
    /// 1588 framing, single-receiver rules.
    pub const SINGLE: PtpOptions = PtpOptions { mode: PtpMode::Single, gptp_framing: false, pair_session: false };

    /// High nibble of every header byte 0 we send.
    fn transport_specific(self) -> u8 {
        if self.gptp_framing {
            TRANSPORT_SPECIFIC_GPTP
        } else {
            TRANSPORT_SPECIFIC_PLAIN
        }
    }
}

/// Start the PTP master serving `receiver_ip` with the single-receiver
/// options. Binds the event (319) and general (320) ports — one session
/// at a time owns them — and starts the announce/sync transmit loop
/// immediately so the clock is live before the RTSP SETUP that
/// advertises it.
pub fn spawn_ptp_master(receiver_ip: IpAddr, local_ip: IpAddr, receiver_name: String) -> Result<PtpMaster> {
    spawn_ptp_master_multi(vec![receiver_ip], local_ip, receiver_name, PtpOptions::SINGLE)
}

/// [`spawn_ptp_master`] for one or several receivers sharing ONE clock —
/// the speakers of a group played together. In the transmitting modes
/// every receiver gets the same unicast Announce/Sync/Follow_Up/Signaling
/// stream and Delay_Req answers, so they all follow one timeline and one
/// anchor means the same instant on each. What happens with the
/// receivers' own clocks is `opts.mode` (see [`PtpMode`]).
pub fn spawn_ptp_master_multi(
    receiver_ips: Vec<IpAddr>,
    local_ip: IpAddr,
    receiver_name: String,
    opts: PtpOptions,
) -> Result<PtpMaster> {
    anyhow::ensure!(!receiver_ips.is_empty(), "PTP master needs at least one receiver");
    let event = bind_ptp(local_ip, PTP_EVENT_PORT).context("bind PTP event :319")?;
    let general = bind_ptp(local_ip, PTP_GENERAL_PORT).context("bind PTP general :320")?;
    event.set_read_timeout(Some(Duration::from_millis(25)))?;
    general.set_nonblocking(true)?;

    let mut rng = rand::thread_rng();
    // Keep the top bit clear: this id crosses three encodings — raw BE
    // bytes in PTP headers, a plist Integer in the SETUP `ClockID`, and
    // `networkTimeTimelineID` in SETRATEANCHORTIME. With bit 63 set, the
    // signed and unsigned plist encodings diverge (i64 cast vs u64) and a
    // strict receiver would see two different timeline ids.
    let clock_id: u64 = rng.gen::<u64>() & 0x7FFF_FFFF_FFFF_FFFF;
    let clock_uuid = format_uuid(rng.gen());

    let timeline = PtpTimeline::new(PtpMasterClock::new(), clock_id, opts.pair_session);
    let stop = Arc::new(AtomicBool::new(false));

    let timeline_t = timeline.clone();
    let stop_t = stop.clone();
    let ips = receiver_ips.clone();
    let handle = thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-ptp:{}", receiver_name))
        .spawn(move || {
            run_master(event, general, ips, timeline_t, stop_t, opts);
        })?;

    let who = receiver_ips.iter().map(|ip| ip.to_string()).collect::<Vec<_>>().join(" + ");
    match opts.mode {
        PtpMode::Follow => info!(
            "AirPlay 2 PTP: follow mode for {} — no Announce/Sync from us; will follow the first \
             member whose Sync/Follow_Up locks (our clock_id={:#018x}, framing {:#04x})",
            who,
            clock_id,
            opts.transport_specific()
        ),
        _ => info!(
            "AirPlay 2 PTP: serving as grandmaster for {} (clock_id={:#018x}, framing {:#04x}{})",
            who,
            clock_id,
            opts.transport_specific(),
            match opts.mode {
                PtpMode::Master => ", members' clocks never followed",
                PtpMode::Single if receiver_ips.len() == 1 => ", following its own clock once it announces one",
                _ => "",
            }
        ),
    }
    Ok(PtpMaster { timeline, clock_uuid, stop, handle: Some(handle) })
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

/// What we know about one receiver's PTP traffic (keyed by source IP):
/// the per-member diagnostics and, in follow mode, its clock.
#[derive(Default)]
struct MemberPtp {
    /// First-of-type bitmask (by message type) for "first X from ip".
    seen: u16,
    /// Last announced (grandmasterIdentity, priority1, clockClass).
    announced: Option<(u64, u8, u8)>,
    /// Its latest Sync (seq, our receive time) awaiting the Follow_Up.
    pending_sync: Option<(u16, u64)>,
    /// Sync's source clock identity (follow mode: the timeline id of a
    /// member that never announces, after [`FOLLOW_ANNOUNCE_WAIT`]).
    sync_clock: u64,
    /// True once one of its Sync/Follow_Up pairs completed.
    locked: bool,
    /// When its first / latest Sync/Follow_Up pair completed.
    first_pair: Option<Instant>,
    last_pair: Option<Instant>,
    /// Follow mode: "Syncs but has not announced" was logged.
    unannounced_logged: bool,
    delay_reqs: u64,
    no_delay_req_logged: bool,
}

/// What the PTP loop learns from the receivers' own packets, and whose
/// clock the timeline follows. Kept apart from the socket loop, with the
/// time passed in, so its transitions are unit-tested.
struct Inbound {
    opts: PtpOptions,
    timeline: PtpTimeline,
    /// [`PtpMode::Single`] with exactly one receiver: that receiver.
    /// Following a receiver's own clock only makes sense for one (several
    /// receivers announce several clocks).
    lone_receiver: Option<IpAddr>,
    members: HashMap<IpAddr, MemberPtp>,
    /// Single receiver: the latest Sync from any source awaiting its
    /// Follow_Up. One slot for the session, as single receivers have
    /// always had.
    single_pending: Option<(u16, u64)>,
    peer_lock_logged: bool,
    /// (member, grandmaster) clocks already logged as not followed.
    announced_peers: Vec<(IpAddr, u64)>,
    /// Follow mode: the member whose clock is followed, once latched.
    latched: Option<IpAddr>,
    /// Follow mode: when the last Delay_Req went to the followed member.
    last_delay_req: Option<Instant>,
    /// Follow mode: "no member is followed" was logged since the last
    /// latch ended.
    no_lock_logged: bool,
}

impl Inbound {
    fn new(opts: PtpOptions, receiver_ips: &[IpAddr], timeline: PtpTimeline) -> Self {
        let lone_receiver = match (opts.mode, receiver_ips) {
            (PtpMode::Single, [ip]) => Some(*ip),
            _ => None,
        };
        Self {
            opts,
            timeline,
            lone_receiver,
            members: receiver_ips.iter().map(|ip| (*ip, MemberPtp::default())).collect(),
            single_pending: None,
            peer_lock_logged: false,
            announced_peers: Vec::new(),
            latched: None,
            last_delay_req: None,
            no_lock_logged: false,
        }
    }

    fn member(&mut self, ip: IpAddr) -> &mut MemberPtp {
        self.members.entry(ip).or_default()
    }

    /// A single receiver: every Announce and Sync/Follow_Up that reaches
    /// us feeds the followed clock, whatever its source.
    fn single_receiver(&self) -> bool {
        self.opts.mode == PtpMode::Single && !self.opts.pair_session && self.lone_receiver.is_some()
    }

    /// [`PtpMode::Single`]: whether `ip`'s Announce and Sync/Follow_Up feed
    /// the followed clock — any source for a single receiver, only the lone
    /// member itself in a one-member pair/group session.
    fn feeds_single_clock(&self, ip: IpAddr) -> bool {
        match self.lone_receiver {
            Some(lone) => !self.opts.pair_session || lone == ip,
            None => false,
        }
    }

    /// A Sync from `ip` (sequence `seq`, source clock identity
    /// `source_clock`), received at `t_recv` on our clock.
    fn on_sync(&mut self, ip: IpAddr, seq: u16, source_clock: u64, t_recv: u64) {
        let m = self.member(ip);
        m.pending_sync = Some((seq, t_recv));
        m.sync_clock = source_clock;
        self.single_pending = Some((seq, t_recv));
    }

    /// An Announce from `ip` naming grandmaster `gm`.
    fn on_announce(&mut self, ip: IpAddr, gm: u64, prio1: u8, class: u8) {
        let m = self.member(ip);
        if m.announced != Some((gm, prio1, class)) {
            info!("AirPlay 2 PTP: {} announces GM {:#018x} prio1 {} class {}", ip, gm, prio1, class);
        }
        m.announced = Some((gm, prio1, class));
        match self.opts.mode {
            PtpMode::Single if self.feeds_single_clock(ip) => {
                let prev = self.timeline.note_announce(gm);
                if prev != gm {
                    info!(
                        "AirPlay 2 PTP: receiver announces its own clock {:#018x} (clockClass {}){}",
                        gm,
                        class,
                        if prev != 0 && self.opts.pair_session { " — it changed; re-locking on its next Sync" } else { "" }
                    );
                }
            }
            PtpMode::Single | PtpMode::Master => {
                if !self.announced_peers.contains(&(ip, gm)) {
                    self.announced_peers.push((ip, gm));
                    info!(
                        "AirPlay 2 PTP: {} announces clock {:#018x} (clockClass {}) — \
                         ignored, the session is anchored on our clock",
                        ip, gm, class
                    );
                }
            }
            PtpMode::Follow => {
                if self.latched == Some(ip) {
                    let prev = self.timeline.note_announce(gm);
                    if prev != gm {
                        info!(
                            "AirPlay 2 PTP: {} now announces GM {:#018x} (was {:#018x}); its previous clock \
                             stays in use until its next Sync/Follow_Up re-locks",
                            ip, gm, prev
                        );
                    }
                }
            }
        }
    }

    /// A Follow_Up from `ip` with sequence `seq` and precise origin
    /// timestamp `t1` (None if unreadable), handled at `now`. Completes
    /// that member's two-step Sync: `t1` and our Sync receive time give
    /// the offset of its clock.
    fn on_follow_up(&mut self, ip: IpAddr, seq: u16, t1: Option<u64>, now: Instant) {
        if self.single_receiver() {
            let Some((pending_seq, t2)) = self.single_pending else { return };
            if pending_seq != seq {
                return;
            }
            self.single_pending = None;
            if let Some(t1) = t1 {
                let got = self.timeline.note_sample(t1, t2);
                self.log_lock(got, ip);
            }
            return;
        }
        let m = self.member(ip);
        let Some((pending_seq, t2)) = m.pending_sync else { return };
        if pending_seq != seq {
            return;
        }
        m.pending_sync = None;
        let Some(t1) = t1 else { return };
        let first_lock = !m.locked;
        m.locked = true;
        let first_pair = *m.first_pair.get_or_insert(now);
        m.last_pair = Some(now);
        // Follow a member only once it has announced its grandmaster, so
        // the id names the timeline its Syncs are on; one that never
        // announces is followed on its Sync identity after a wait.
        let follow_id = match m.announced {
            Some(a) => Some(a.0),
            None if now.saturating_duration_since(first_pair) >= FOLLOW_ANNOUNCE_WAIT => Some(m.sync_clock),
            None => None,
        };
        let announced = m.announced.is_some();
        match self.opts.mode {
            PtpMode::Single if self.lone_receiver == Some(ip) => {
                let got = self.timeline.note_sample(t1, t2);
                self.log_lock(got, ip);
            }
            PtpMode::Follow => match (self.latched, follow_id) {
                (None, Some(id)) => {
                    self.latched = Some(ip);
                    self.last_delay_req = None;
                    let off = self.timeline.relock(id, t1, t2).offset();
                    info!(
                        "AirPlay 2 PTP: following GM {:#018x} of {} (offset {:.3} s){} — anchors and \
                         sync packets use its timeline",
                        id,
                        ip,
                        off as f64 / 1e9,
                        if announced { "" } else { " — it never announced; its Sync identity names the timeline" }
                    );
                }
                (None, None) => {
                    let m = self.member(ip);
                    if !m.unannounced_logged {
                        m.unannounced_logged = true;
                        info!(
                            "AirPlay 2 PTP: {} Syncs to us but has not announced its grandmaster; \
                             following it once it does (or on its Sync identity after {} s)",
                            ip,
                            FOLLOW_ANNOUNCE_WAIT.as_secs()
                        );
                    }
                }
                (Some(l), _) if l == ip => {
                    let got = self.timeline.note_sample(t1, t2);
                    if let PeerSample::Locked(off) = got {
                        info!("AirPlay 2 PTP: re-locked to {}'s clock (offset {:.3} s)", ip, off as f64 / 1e9);
                    }
                    log_peer_step(got, ip);
                }
                (Some(l), id) => {
                    if first_lock {
                        info!(
                            "AirPlay 2 PTP: {} also Syncs to us (GM {}) — not followed; following {}",
                            ip,
                            id.map(|i| format!("{:#018x}", i)).unwrap_or_else(|| "not announced".into()),
                            l
                        );
                    }
                }
            },
            _ => {}
        }
    }

    /// Log a (re)lock onto the followed receiver's clock, and any step.
    fn log_lock(&mut self, got: PeerSample, ip: IpAddr) {
        if let PeerSample::Locked(off) = got {
            if !self.peer_lock_logged {
                self.peer_lock_logged = true;
                info!(
                    "AirPlay 2 PTP: locked to the receiver's clock (offset {:.3} s) — can anchor on its timeline",
                    off as f64 / 1e9
                );
            } else {
                info!("AirPlay 2 PTP: re-locked to the receiver's clock (offset {:.3} s)", off as f64 / 1e9);
            }
        }
        log_peer_step(got, ip);
    }

    /// Follow mode: stop following a member that completed no
    /// Sync/Follow_Up pair for [`FOLLOW_STALE`] by `now`, so the next
    /// member whose pair arrives is followed. Its last clock stays
    /// published — anchors and sync packets keep naming a clock that
    /// exists — until then.
    fn check_stale(&mut self, now: Instant) {
        let (PtpMode::Follow, Some(ip)) = (self.opts.mode, self.latched) else { return };
        let quiet = self.members.get(&ip).and_then(|m| m.last_pair).map(|t| now.saturating_duration_since(t));
        if quiet.map_or(false, |q| q >= FOLLOW_STALE) {
            warn!(
                "AirPlay 2 PTP: follow mode — {} sent no Sync/Follow_Up for {} s; no longer following it. \
                 Anchors and sync packets stay on its last clock{} until another member's Sync/Follow_Up \
                 arrives",
                ip,
                FOLLOW_STALE.as_secs(),
                self.timeline.followed().map(|(id, _)| format!(" (GM {:#018x})", id)).unwrap_or_default()
            );
            self.latched = None;
            self.no_lock_logged = false;
        }
    }

    /// Follow mode: the followed member, when a Delay_Req to it is due at
    /// `now` (the send is recorded).
    fn delay_req_due(&mut self, now: Instant) -> Option<IpAddr> {
        if self.opts.mode != PtpMode::Follow {
            return None;
        }
        let ip = self.latched?;
        if self.last_delay_req.map_or(true, |t| now.saturating_duration_since(t) >= DELAY_REQ_INTERVAL) {
            self.last_delay_req = Some(now);
            Some(ip)
        } else {
            None
        }
    }
}

fn run_master(
    event: UdpSocket,
    general: UdpSocket,
    receiver_ips: Vec<IpAddr>,
    timeline: PtpTimeline,
    stop: Arc<AtomicBool>,
    opts: PtpOptions,
) {
    let clock_id = timeline.clock_id;
    let clock = timeline.clock.clone();
    let ts = opts.transport_specific();
    let event_dsts: Vec<SocketAddr> =
        receiver_ips.iter().map(|ip| SocketAddr::new(*ip, PTP_EVENT_PORT)).collect();
    let general_dsts: Vec<SocketAddr> =
        receiver_ips.iter().map(|ip| SocketAddr::new(*ip, PTP_GENERAL_PORT)).collect();
    // Follow mode sends nothing; the other modes are the grandmaster.
    let transmit = opts.mode != PtpMode::Follow;
    let receivers_label = receiver_ips.iter().map(|ip| ip.to_string()).collect::<Vec<_>>().join(" + ");
    let mut inbound = Inbound::new(opts, &receiver_ips, timeline.clone());
    let mut delay_req_seq: u16 = 0;
    let started = Instant::now();

    let mut announce_seq: u16 = 0;
    let mut sync_seq: u16 = 0;
    let mut signaling_seq: u16 = 0;
    let mut last_announce: Option<Instant> = None;
    let mut last_sync: Option<Instant> = None;
    let mut last_signaling: Option<Instant> = None;
    let mut delay_resps: u64 = 0;
    let mut rx_total: u64 = 0;
    let mut last_status = Instant::now();
    let mut buf = [0u8; 256];

    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();

        if transmit && last_announce.map_or(true, |t| now.duration_since(t) >= ANNOUNCE_INTERVAL) {
            let pkt = build_announce(ts, clock_id, announce_seq);
            for dst in &general_dsts {
                let _ = general.send_to(&pkt, dst);
            }
            announce_seq = announce_seq.wrapping_add(1);
            last_announce = Some(now);
        }

        // Apple-proprietary signaling TLVs — libairptp sends these to every
        // peer from the moment it's added (1 s cadence). Observed from iOS
        // senders; receivers appear to expect them before engaging a master.
        if transmit && last_signaling.map_or(true, |t| now.duration_since(t) >= SIGNALING_INTERVAL) {
            let pkt = build_signaling(ts, clock_id, signaling_seq);
            for dst in &general_dsts {
                let _ = general.send_to(&pkt, dst);
            }
            signaling_seq = signaling_seq.wrapping_add(1);
            last_signaling = Some(now);
        }

        if transmit && last_sync.map_or(true, |t| now.duration_since(t) >= SYNC_INTERVAL) {
            // Two-step: Sync carries a coarse origin, the Follow_Up sent
            // right behind it carries the precise origin timestamp.
            // Each receiver gets its own Sync + Follow_Up pair so the
            // precise origin timestamp is the one its Sync left with (a
            // shared Follow_Up would be off by the inter-send gap).
            for (event_dst, general_dst) in event_dsts.iter().zip(&general_dsts) {
                let coarse = clock.now_ns();
                let sync = build_sync(ts, clock_id, sync_seq, coarse);
                let _ = event.send_to(&sync, event_dst);
                let precise = clock.now_ns();
                let fup = build_follow_up(ts, clock_id, sync_seq, precise);
                let _ = general.send_to(&fup, general_dst);
            }
            sync_seq = sync_seq.wrapping_add(1);
            last_sync = Some(now);
        }

        // Follow mode: behave like a PTP slave of the member we follow —
        // Delay_Req to its event port, the only message follow mode sends.
        if let Some(ip) = inbound.delay_req_due(now) {
            let req = build_delay_req(ts, clock_id, delay_req_seq);
            if let Err(e) = event.send_to(&req, SocketAddr::new(ip, PTP_EVENT_PORT)) {
                debug!("AP2 PTP: Delay_Req to {} failed: {}", ip, e);
            } else if delay_req_seq == 0 {
                info!("AirPlay 2 PTP: follow mode — sending Delay_Req to {} every {} s", ip, DELAY_REQ_INTERVAL.as_secs());
            }
            delay_req_seq = delay_req_seq.wrapping_add(1);
        }

        // Event port: the receiver's Delay_Req (and its own Sync) arrive
        // here. The 25 ms read timeout doubles as the loop tick. Every
        // first message of a type is logged per member, so the log shows
        // exactly how each receiver engages.
        match event.recv_from(&mut buf) {
            Ok((n, src)) => {
                let t_recv = clock.now_ns();
                rx_total += 1;
                if let Some(h) = parse_header(&buf[..n]) {
                    let ip = src.ip();
                    note_inbound(&mut inbound.member(ip).seen, h.msg_type, PTP_EVENT_PORT, src);
                    match h.msg_type {
                        MSG_DELAY_REQ => {
                            let resp = build_delay_resp(
                                ts,
                                clock_id,
                                h.sequence_id,
                                t_recv,
                                &h.source_port_identity,
                            );
                            let _ = general.send_to(&resp, SocketAddr::new(ip, PTP_GENERAL_PORT));
                            delay_resps += 1;
                            inbound.member(ip).delay_reqs += 1;
                            if delay_resps == 1 {
                                info!("AirPlay 2 PTP: receiver {} is exchanging Delay_Req — clock lock in progress", ip);
                            }
                        }
                        // The receiver's own Sync (it masters its clock at
                        // us): remember (seq, our recv time) and pair it
                        // with the Follow_Up's precise origin timestamp.
                        MSG_SYNC => {
                            let mut id = [0u8; 8];
                            id.copy_from_slice(&h.source_port_identity[..8]);
                            inbound.on_sync(ip, h.sequence_id, u64::from_be_bytes(id), t_recv);
                        }
                        _ => {}
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

        // General port: Announce / Follow_Up / Signaling / Delay_Resp from
        // the receivers.
        while let Ok((n, src)) = general.recv_from(&mut buf) {
            rx_total += 1;
            let Some(h) = parse_header(&buf[..n]) else { continue };
            let ip = src.ip();
            note_inbound(&mut inbound.member(ip).seen, h.msg_type, PTP_GENERAL_PORT, src);
            match h.msg_type {
                MSG_ANNOUNCE => {
                    if let Some((gm, prio1, class)) = parse_announce(&buf[..n]) {
                        inbound.on_announce(ip, gm, prio1, class);
                    }
                }
                MSG_FOLLOW_UP => {
                    inbound.on_follow_up(ip, h.sequence_id, read_timestamp(&buf[..n], HEADER_LEN), Instant::now())
                }
                _ => {}
            }
        }

        inbound.check_stale(Instant::now());

        // Heartbeat: if the receiver never talks back, the PTP clock can't
        // lock and playback is silent — say so loudly, with the likely cause.
        if last_status.elapsed() >= Duration::from_secs(5) {
            if rx_total == 0 {
                warn!(
                    "AirPlay 2 PTP: receiver {} has sent us NOTHING on 319/320 after 5s — it isn't \
                     engaging our clock. Likely inbound UDP 319/320 is firewall-blocked, or it \
                     expects unicast-PTP signaling we don't send yet. No clock lock = silent audio.",
                    receivers_label
                );
            } else {
                // Delay_Resp count is only interesting when nonzero:
                // receivers that master their own clock (Sonos) never send
                // Delay_Req, so 0 is the normal state, not a failure.
                if delay_resps > 0 {
                    info!(
                        "AirPlay 2 PTP status: {} packet(s) from receiver, {} Delay_Resp served",
                        rx_total, delay_resps
                    );
                } else {
                    info!("AirPlay 2 PTP status: {} packet(s) from receiver", rx_total);
                }
            }
            // Per member: a receiver that follows our grandmaster sends
            // Delay_Req; one that never does is not taking our clock.
            if transmit && started.elapsed() >= Duration::from_secs(5) {
                for ip in &receiver_ips {
                    let m = inbound.member(*ip);
                    if m.delay_reqs == 0 && !m.no_delay_req_logged {
                        m.no_delay_req_logged = true;
                        info!(
                            "AirPlay 2 PTP: NO Delay_Req from {} after 5 s{}",
                            ip,
                            match m.announced {
                                Some((gm, p, c)) => format!(
                                    " (it announces its own GM {:#018x} prio1 {} class {})",
                                    gm, p, c
                                ),
                                None => String::new(),
                            }
                        );
                    }
                }
            }
            if opts.mode == PtpMode::Follow && inbound.latched.is_none() && !inbound.no_lock_logged {
                inbound.no_lock_logged = true;
                match timeline.followed() {
                    Some((id, _)) => warn!(
                        "AirPlay 2 PTP: follow mode — no member is followed; anchors and sync packets stay \
                         on the last followed clock (GM {:#018x}) until a member's Sync/Follow_Up arrives",
                        id
                    ),
                    None => warn!(
                        "AirPlay 2 PTP: follow mode — no member is followed after 5 s (none completed a \
                         Sync/Follow_Up pair after announcing its grandmaster); realtime sync packets fall \
                         back to our clock, which we are not serving, and the buffered anchor waits"
                    ),
                }
            }
            last_status = Instant::now();
        }
    }
    debug!("AirPlay 2 PTP master exiting ({} delay-resps served)", delay_resps);
}

/// Log a step of the followed clock (a held outlier at debug).
fn log_peer_step(got: PeerSample, ip: IpAddr) {
    match got {
        PeerSample::Stepped(off) => info!(
            "AirPlay 2 PTP: {}'s clock stepped — re-locked on the new offset {:.3} s",
            ip,
            off as f64 / 1e9
        ),
        PeerSample::Held(off) => debug!(
            "AP2 PTP: {} offset sample {:.3} s is far from the lock; held until a second agrees",
            ip,
            off as f64 / 1e9
        ),
        PeerSample::Locked(_) | PeerSample::Tracked(_) => {}
    }
}

/// Log inbound PTP traffic: the first packet of each message type per
/// receiver at INFO (so a default log shows exactly how every receiver
/// engages), repeats at debug. `seen` is the member's bitmask indexed by
/// message type.
fn note_inbound(seen: &mut u16, msg_type: u8, port: u16, src: SocketAddr) {
    let name = match msg_type {
        0x0 => "Sync",
        0x1 => "Delay_Req",
        0x8 => "Follow_Up",
        0x9 => "Delay_Resp",
        0xB => "Announce",
        0xC => "Signaling",
        _ => "other",
    };
    let bit = 1u16 << (msg_type & 0x0f);
    if *seen & bit == 0 {
        *seen |= bit;
        info!("AirPlay 2 PTP: first {} (type {:#x}) from {} on :{}", name, msg_type, src, port);
    } else {
        debug!("AP2 PTP: {} from {} on :{}", name, src, port);
    }
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
    ts: u8,
    msg_type: u8,
    flags: u16,
    clock_id: u64,
    seq: u16,
    body_len: usize,
    control: u8,
    log_interval: i8,
) {
    let total = HEADER_LEN + body_len;
    // transportSpecific (majorSdoId), high nibble: 1 = gPTP (802.1AS)
    // framing — libairptp sets `type | 0x10` ("TranSpec = 1 which is
    // expected by nqptp") and airplay-cli reports that receivers discard
    // plain-1588 framing; pair/group sessions send it. 0 = plain 1588, what
    // single receivers have always been sent, and they play (kept unless
    // the config forces gPTP).
    out.push((ts & 0xf0) | (msg_type & 0x0f));
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

/// Append one TLV: tlvType(2 BE) + lengthField(2 BE) + value bytes.
fn push_tlv(out: &mut Vec<u8>, tlv_type: u16, value: &[u8]) {
    out.extend_from_slice(&tlv_type.to_be_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
}

fn build_announce(ts: u8, clock_id: u64, seq: u16) -> Vec<u8> {
    // Body: originTimestamp(10) currentUtcOffset(2) reserved(1)
    // grandmasterPriority1(1) grandmasterClockQuality(4)
    // grandmasterPriority2(1) grandmasterIdentity(8) stepsRemoved(2)
    // timeSource(1) = 30 bytes, then a PATH_TRACE TLV (4+8) carrying our
    // clock identity — libairptp appends it and Apple gear expects it.
    let body_len = 30 + 4 + 8;
    let mut out = Vec::with_capacity(HEADER_LEN + body_len);
    build_header(&mut out, ts, MSG_ANNOUNCE, FLAGS_GENERAL, clock_id, seq, body_len, 0x05, LOG_INTERVAL_ANNOUNCE);
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
    push_tlv(&mut out, TLV_PATH_TRACE, &clock_id.to_be_bytes());
    out
}

/// Apple-proprietary PTP Signaling message, byte-for-byte per libairptp's
/// `msg_signaling_make`: targetPortIdentity all-zero, then two Apple
/// org-extension TLVs (subtypes 1 and 5) whose payloads start with the
/// fixed `00 00 03 01` and are zero-padded to 22/32 value bytes.
fn build_signaling(ts: u8, clock_id: u64, seq: u16) -> Vec<u8> {
    let tlv_value = |subtype: u8, value_len: usize| -> Vec<u8> {
        let mut v = Vec::with_capacity(value_len);
        v.extend_from_slice(&ORG_APPLE);
        v.extend_from_slice(&[0x00, 0x00, subtype]);
        v.extend_from_slice(&APPLE_UNKNOWN);
        v.resize(value_len, 0);
        v
    };
    let tlv1 = tlv_value(0x01, 22);
    let tlv2 = tlv_value(0x05, 32);
    let body_len = 10 + (4 + tlv1.len()) + (4 + tlv2.len());
    let mut out = Vec::with_capacity(HEADER_LEN + body_len);
    build_header(&mut out, ts, MSG_SIGNALING, FLAGS_GENERAL, clock_id, seq, body_len, 0x05, LOG_INTERVAL_SIGNALING);
    out.extend_from_slice(&[0u8; 10]); // targetPortIdentity (zeros; iOS fills it, libairptp doesn't)
    push_tlv(&mut out, TLV_ORG_EXTENSION, &tlv1);
    push_tlv(&mut out, TLV_ORG_EXTENSION, &tlv2);
    out
}

fn build_sync(ts: u8, clock_id: u64, seq: u16, origin_ns: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 10);
    build_header(&mut out, ts, MSG_SYNC, FLAGS_SYNC, clock_id, seq, 10, 0x00, LOG_INTERVAL_SYNC);
    write_timestamp(&mut out, origin_ns);
    out
}

fn build_follow_up(ts: u8, clock_id: u64, seq: u16, origin_ns: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 10);
    build_header(&mut out, ts, MSG_FOLLOW_UP, FLAGS_GENERAL, clock_id, seq, 10, 0x02, LOG_INTERVAL_SYNC);
    write_timestamp(&mut out, origin_ns);
    out
}

fn build_delay_resp(ts: u8, clock_id: u64, seq: u16, receive_ns: u64, requesting_port_identity: &[u8; 10]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 20);
    build_header(&mut out, ts, MSG_DELAY_RESP, FLAGS_GENERAL, clock_id, seq, 20, 0x03, LOG_INTERVAL_DELAY_RESP);
    write_timestamp(&mut out, receive_ns); // receiveTimestamp
    out.extend_from_slice(requesting_port_identity);
    out
}

/// Delay_Req (event port) — sent to the member we follow in follow mode,
/// as a PTP slave of that member does.
fn build_delay_req(ts: u8, clock_id: u64, seq: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + 10);
    build_header(&mut out, ts, MSG_DELAY_REQ, FLAG_UNICAST, clock_id, seq, 10, 0x01, 0x7f);
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

/// Pull (grandmasterIdentity, priority1, clockClass) out of an inbound
/// Announce. Body offsets after the 34-byte header: originTimestamp(10),
/// currentUtcOffset(2), reserved(1), priority1(1), clockQuality(4),
/// priority2(1), grandmasterIdentity(8).
fn parse_announce(buf: &[u8]) -> Option<(u64, u8, u8)> {
    if buf.len() < HEADER_LEN + 30 {
        return None;
    }
    let prio1 = buf[HEADER_LEN + 13];
    let class = buf[HEADER_LEN + 14];
    let mut id = [0u8; 8];
    id.copy_from_slice(&buf[HEADER_LEN + 19..HEADER_LEN + 27]);
    Some((u64::from_be_bytes(id), prio1, class))
}

/// Read a 10-byte PTP timestamp at `off`, returning ns.
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
        let sync = build_sync(TRANSPORT_SPECIFIC_GPTP, 0x0102030405060708, 0x1234, 5_000_000_123);
        let h = parse_header(&sync).unwrap();
        assert_eq!(h.msg_type, MSG_SYNC);
        assert_eq!(h.sequence_id, 0x1234);
        // gPTP framing: transportSpecific=1 in the high nibble of byte 0
        // (libairptp `type | 0x10`) — what pair/group sessions send.
        assert_eq!(sync[0], 0x10 | MSG_SYNC);
        assert_eq!(build_announce(TRANSPORT_SPECIFIC_GPTP, 1, 0)[0], 0x10 | MSG_ANNOUNCE);
        // clockIdentity BE + portNumber 0x8005.
        assert_eq!(&h.source_port_identity[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&h.source_port_identity[8..], &PORT_NUMBER);
        // messageLength = header + 10-byte timestamp.
        assert_eq!(u16::from_be_bytes([sync[2], sync[3]]), (HEADER_LEN + 10) as u16);
        // Sync flags: UNICAST | TIMESCALE | TWO_STEP = 0x0608.
        assert_eq!(u16::from_be_bytes([sync[6], sync[7]]), 0x0608);
        // Follow_Up drops TWO_STEP → 0x0408.
        let fup = build_follow_up(TRANSPORT_SPECIFIC_GPTP, 1, 1, 0);
        assert_eq!(u16::from_be_bytes([fup[6], fup[7]]), 0x0408);
    }

    #[test]
    fn single_receivers_keep_plain_1588_framing_unless_forced() {
        // The framing byte of every message kind we send, per options:
        // single receivers get transportSpecific 0, pair/group sessions —
        // or a forced single — 1.
        let single = PtpOptions::SINGLE.transport_specific();
        let group = PtpOptions { mode: PtpMode::Master, gptp_framing: true, pair_session: true }.transport_specific();
        let forced = PtpOptions { gptp_framing: true, ..PtpOptions::SINGLE }.transport_specific();
        assert_eq!(single, 0x00);
        assert_eq!(group, 0x10);
        assert_eq!(forced, 0x10);
        let pid = [0u8; 10];
        for (ts, want) in [(single, 0x00), (group, 0x10)] {
            assert_eq!(build_sync(ts, 1, 0, 0)[0], want | MSG_SYNC);
            assert_eq!(build_follow_up(ts, 1, 0, 0)[0], want | MSG_FOLLOW_UP);
            assert_eq!(build_announce(ts, 1, 0)[0], want | MSG_ANNOUNCE);
            assert_eq!(build_signaling(ts, 1, 0)[0], want | MSG_SIGNALING);
            assert_eq!(build_delay_resp(ts, 1, 0, 0, &pid)[0], want | MSG_DELAY_RESP);
            // Only the high nibble differs; the parser ignores it.
            assert_eq!(parse_header(&build_sync(ts, 1, 7, 0)).unwrap().sequence_id, 7);
        }
        assert_eq!(PtpOptions::SINGLE.mode, PtpMode::Single);
    }

    #[test]
    fn announce_matches_libairptp_grandmaster_fields() {
        let id = 0xAABBCCDDEEFF0011u64;
        let ann = build_announce(TRANSPORT_SPECIFIC_GPTP, id, 7);
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
        // PATH_TRACE TLV: type 0x0008, length 8, value = clock identity.
        assert_eq!(u16::from_be_bytes([ann[64], ann[65]]), 0x0008);
        assert_eq!(u16::from_be_bytes([ann[66], ann[67]]), 8);
        assert_eq!(&ann[68..76], &id.to_be_bytes());
        assert_eq!(ann.len(), HEADER_LEN + 30 + 12);
        // messageLength reflects the TLV too.
        assert_eq!(u16::from_be_bytes([ann[2], ann[3]]), ann.len() as u16);
    }

    #[test]
    fn signaling_matches_libairptp_layout() {
        let sig = build_signaling(TRANSPORT_SPECIFIC_GPTP, 0x1122334455667788, 9);
        let h = parse_header(&sig).unwrap();
        assert_eq!(h.msg_type, MSG_SIGNALING);
        assert_eq!(sig.len(), HEADER_LEN + 10 + 26 + 36); // 106
        assert_eq!(u16::from_be_bytes([sig[2], sig[3]]), 106);
        assert_eq!(u16::from_be_bytes([sig[6], sig[7]]), 0x0408); // UNICAST|TIMESCALE
        assert_eq!(sig[33], 0x80); // logMessageInterval = -128
        assert_eq!(&sig[34..44], &[0u8; 10]); // targetPortIdentity zeros
        // TLV1: org-extension, len 22, Apple OUI, subtype 1, fixed prefix.
        assert_eq!(u16::from_be_bytes([sig[44], sig[45]]), 0x0003);
        assert_eq!(u16::from_be_bytes([sig[46], sig[47]]), 22);
        assert_eq!(&sig[48..51], &[0x00, 0x0d, 0x93]);
        assert_eq!(&sig[51..54], &[0x00, 0x00, 0x01]);
        assert_eq!(&sig[54..58], &[0x00, 0x00, 0x03, 0x01]);
        assert!(sig[58..70].iter().all(|&b| b == 0)); // zero padding
        // TLV2: len 32, subtype 5, same fixed prefix.
        assert_eq!(u16::from_be_bytes([sig[70], sig[71]]), 0x0003);
        assert_eq!(u16::from_be_bytes([sig[72], sig[73]]), 32);
        assert_eq!(&sig[74..77], &[0x00, 0x0d, 0x93]);
        assert_eq!(&sig[77..80], &[0x00, 0x00, 0x05]);
        assert_eq!(&sig[80..84], &[0x00, 0x00, 0x03, 0x01]);
        assert!(sig[84..106].iter().all(|&b| b == 0));
    }

    #[test]
    fn announce_grandmaster_roundtrips_through_parser() {
        let id = 0x0123456789ABCDEFu64;
        let ann = build_announce(TRANSPORT_SPECIFIC_GPTP, id, 3);
        let (gm, prio1, class) = parse_announce(&ann).unwrap();
        assert_eq!(gm, id);
        assert_eq!(prio1, 128);
        assert_eq!(class, 0x06);
    }

    #[test]
    fn delay_resp_echoes_seq_and_requesting_identity() {
        let req = build_delay_req(TRANSPORT_SPECIFIC_GPTP, 0x1111111111111111, 0x0042);
        let h = parse_header(&req).unwrap();
        let resp = build_delay_resp(TRANSPORT_SPECIFIC_GPTP, 0x2222222222222222, h.sequence_id, 1_500_000_000, &h.source_port_identity);
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
    fn timeline_follows_receiver_clock() {
        for pair_rules in [false, true] {
            let tl = PtpTimeline::new(PtpMasterClock::new(), 0x42, pair_rules);
            let later = Instant::now() + Duration::from_secs(2);
            // Nothing followed yet: our own clock.
            assert!(tl.receiver_now_ns().is_none());
            assert_eq!(tl.time_at(later), (0x42, tl.clock.ns_at(later)));
            tl.note_announce(0xABCDEF);
            assert!(tl.receiver_now_ns().is_none()); // announced but not locked
            // Receiver clock ~1000 s ahead of ours.
            let t2 = tl.our_now_ns();
            let t1 = t2 + 1_000_000_000_000;
            assert_eq!(tl.note_sample(t1, t2), PeerSample::Locked(1_000_000_000_000));
            let (id, now) = tl.receiver_now_ns().unwrap();
            assert_eq!(id, 0xABCDEF);
            // receiver_now ≈ our_now + offset (allow scheduling slack).
            let expect = tl.our_now_ns() + 1_000_000_000_000;
            assert!((now as i64 - expect as i64).abs() < 50_000_000);
            // time_at: any instant, on the followed clock.
            assert_eq!(tl.time_at(later), (0xABCDEF, tl.clock.ns_at(later) + 1_000_000_000_000));
        }
    }

    #[test]
    fn a_single_receiver_keeps_its_offset_under_a_new_grandmaster_id() {
        // The single-receiver rule: the announced id is swapped in place
        // and the offset carries over, converging sample by sample.
        let mut p = PeerClock::new(false);
        p.announce(0xA);
        assert_eq!(p.sample(1_000_000_000_000), PeerSample::Locked(1_000_000_000_000));
        assert_eq!(p.announce(0xB), 0xA);
        assert_eq!(p.published(), Some((0xB, 1_000_000_000_000)));
        assert_eq!(p.sample(1_000_000_000_000), PeerSample::Tracked(1_000_000_000_000));
        assert_eq!(p.epoch, 1, "one lock, never relocked");
    }

    #[test]
    fn a_single_receiver_blends_every_sample() {
        // No outlier hold: an 80 ms sample moves the smoothed offset by an
        // eighth of the difference, as single receivers always have.
        let mut p = PeerClock::new(false);
        p.announce(0xA);
        p.sample(5_000_000_000);
        assert_eq!(p.sample(4_920_000_000), PeerSample::Tracked(4_990_000_000));
        assert_eq!(p.published(), Some((0xA, 4_990_000_000)));
        // Even a jump of seconds is blended, not relocked.
        assert_eq!(p.sample(900_000_000_000), PeerSample::Tracked(116_866_250_000));
        assert_eq!(p.epoch, 1);
    }

    #[test]
    fn pair_rules_never_publish_the_new_id_with_the_old_offset() {
        const OLD: u64 = 0xA;
        const NEW: u64 = 0xB;
        let mut p = PeerClock::new(true);
        p.announce(OLD);
        assert_eq!(p.sample(1_000_000_000_000), PeerSample::Locked(1_000_000_000_000));
        assert_eq!(p.published(), Some((OLD, 1_000_000_000_000)));
        // The followed member announces another grandmaster: the old
        // clock (id AND offset) stays published …
        assert_eq!(p.announce(NEW), OLD);
        assert_eq!(p.published(), Some((OLD, 1_000_000_000_000)));
        // … until the first Sync/Follow_Up after the change: its raw
        // sample on the new id, a new epoch.
        assert_eq!(p.sample(7_000_000_000), PeerSample::Locked(7_000_000_000));
        assert_eq!(p.published(), Some((NEW, 7_000_000_000)));
        assert_eq!(p.epoch, 2);
        // The same id again changes nothing.
        assert_eq!(p.announce(NEW), NEW);
        assert_eq!(p.published(), Some((NEW, 7_000_000_000)));
        // A change announced and reverted before any sample: tracking goes on.
        p.announce(OLD);
        p.announce(NEW);
        assert_eq!(p.sample(7_000_000_800), PeerSample::Tracked(7_000_000_100));
        assert_eq!(p.epoch, 2);
    }

    #[test]
    fn pair_rules_take_a_step_raw_once_confirmed_and_drop_a_lone_outlier() {
        let mut p = PeerClock::new(true);
        p.announce(0xA);
        p.sample(5_000_000_000);
        // Within the step limit: blended (7/8 old + 1/8 new).
        assert_eq!(p.sample(5_000_800_000), PeerSample::Tracked(5_000_100_000));
        // One sample far off (a delayed packet): held, not blended.
        assert_eq!(p.sample(5_900_000_000), PeerSample::Held(5_900_000_000));
        assert_eq!(p.published(), Some((0xA, 5_000_100_000)));
        // The next agrees with the lock again: the outlier is forgotten.
        assert_eq!(p.sample(5_000_100_000), PeerSample::Tracked(5_000_100_000));
        // A real step (new timescale, its Announce still on the way): two
        // samples that agree with each other → relocked on the raw value,
        // not converged over tens of seconds.
        assert_eq!(p.sample(900_000_000_000), PeerSample::Held(900_000_000_000));
        assert_eq!(p.sample(900_000_001_000), PeerSample::Stepped(900_000_001_000));
        assert_eq!(p.published(), Some((0xA, 900_000_001_000)));
        // Two locks so far (the first, the step); tracking keeps the epoch.
        assert_eq!(p.epoch, 2);
        p.sample(900_000_002_000);
        assert_eq!(p.epoch, 2);
    }

    #[test]
    fn a_relock_replaces_the_followed_clock_in_one_step_with_a_new_epoch() {
        let tl = PtpTimeline::new(PtpMasterClock::new(), 1, true);
        tl.note_announce(0xA);
        tl.note_sample(10, 0);
        let (_, _, e1) = tl.followed_epoch().unwrap();
        assert_eq!(tl.relock(0xB, 30, 0), PeerSample::Locked(30));
        let (id, off, e2) = tl.followed_epoch().unwrap();
        assert_eq!((id, off), (0xB, 30));
        assert_ne!(e1, e2, "an anchor made before the relock must see it");
    }

    #[test]
    fn samples_before_the_first_announce_lock_under_its_id() {
        // A lone receiver's Syncs can arrive before its first Announce:
        // the offset is kept (unpublished) and the first Announce names
        // it, under either rules.
        for pair_rules in [false, true] {
            let mut p = PeerClock::new(pair_rules);
            assert_eq!(p.sample(42), PeerSample::Locked(42));
            assert_eq!(p.published(), None);
            p.announce(0xC);
            assert_eq!(p.published(), Some((0xC, 42)));
            assert_eq!(p.epoch, 1);
        }
    }

    // ---- Per-packet transitions (the run_master loop minus its sockets).

    fn addr(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn inbound(opts: PtpOptions, ips: &[IpAddr]) -> Inbound {
        Inbound::new(opts, ips, PtpTimeline::new(PtpMasterClock::new(), 0x42, opts.pair_session))
    }

    /// One Sync + Follow_Up from `ip`: the member's clock read `t1` when
    /// our clock read `t2`, completed at `now`.
    fn pair_from(inb: &mut Inbound, ip: IpAddr, seq: u16, sync_clock: u64, t1: u64, t2: u64, now: Instant) {
        inb.on_sync(ip, seq, sync_clock, t2);
        inb.on_follow_up(ip, seq, Some(t1), now);
    }

    const S: u64 = 1_000_000_000;

    #[test]
    fn a_single_receiver_follows_every_source_with_the_single_receiver_rules() {
        let rx = addr("192.0.2.10");
        let other = addr("192.0.2.99");
        let now = Instant::now();
        let mut inb = inbound(PtpOptions::SINGLE, &[rx]);
        // Any source's Announce and Sync/Follow_Up feed the clock.
        inb.on_announce(other, 0xA, 248, 248);
        pair_from(&mut inb, other, 1, 0xA, 1000 * S, 0, now);
        assert_eq!(inb.timeline.followed(), Some((0xA, 1000 * S as i64)));
        // One pending Sync for the session: a Follow_Up completes the
        // latest Sync whatever its source, a stale sequence does nothing.
        inb.on_sync(rx, 7, 0xA, 10 * S);
        inb.on_follow_up(other, 6, Some(0), now);
        inb.on_follow_up(other, 7, Some(1010 * S), now);
        assert_eq!(inb.timeline.followed(), Some((0xA, 1000 * S as i64)));
        // A new grandmaster id keeps the offset; samples are blended.
        inb.on_announce(rx, 0xB, 248, 248);
        assert_eq!(inb.timeline.followed(), Some((0xB, 1000 * S as i64)));
        pair_from(&mut inb, rx, 8, 0xB, 1000 * S - 80_000_000, 0, now);
        assert_eq!(inb.timeline.followed(), Some((0xB, 1000 * S as i64 - 10_000_000)));
        assert_eq!(inb.timeline.followed_epoch().unwrap().2, 1);
        // Nothing else happens in single mode.
        assert_eq!(inb.latched, None);
        assert_eq!(inb.delay_req_due(now), None);
    }

    #[test]
    fn a_one_member_pair_session_follows_only_that_member() {
        let rx = addr("192.0.2.10");
        let buddy = addr("192.0.2.11");
        let now = Instant::now();
        let opts = PtpOptions { mode: PtpMode::Single, gptp_framing: true, pair_session: true };
        let mut inb = inbound(opts, &[rx]);
        // The pair's other half announces and Syncs to us: ignored.
        inb.on_announce(buddy, 0xB, 248, 248);
        pair_from(&mut inb, buddy, 1, 0xB, 500 * S, 0, now);
        assert_eq!(inb.timeline.followed(), None);
        // The member itself: followed.
        inb.on_announce(rx, 0xA, 248, 248);
        pair_from(&mut inb, rx, 1, 0xA, 1000 * S, 0, now);
        assert_eq!(inb.timeline.followed(), Some((0xA, 1000 * S as i64)));
        // A lone 80 ms outlier is held, not blended.
        pair_from(&mut inb, rx, 2, 0xA, 1000 * S - 80_000_000, 0, now);
        assert_eq!(inb.timeline.followed(), Some((0xA, 1000 * S as i64)));
        // A grandmaster change: the old clock until its next sample.
        inb.on_announce(rx, 0xC, 248, 248);
        assert_eq!(inb.timeline.followed(), Some((0xA, 1000 * S as i64)));
        pair_from(&mut inb, rx, 3, 0xC, 20 * S, 0, now);
        assert_eq!(inb.timeline.followed(), Some((0xC, 20 * S as i64)));
    }

    #[test]
    fn master_mode_never_follows_a_member() {
        let (a, b) = (addr("192.0.2.10"), addr("192.0.2.11"));
        let now = Instant::now();
        let opts = PtpOptions { mode: PtpMode::Master, gptp_framing: true, pair_session: true };
        let mut inb = inbound(opts, &[a, b]);
        inb.on_announce(a, 0xA, 248, 248);
        pair_from(&mut inb, a, 1, 0xA, 1000 * S, 0, now);
        assert_eq!(inb.timeline.followed(), None);
        assert_eq!(inb.latched, None);
        assert_eq!(inb.delay_req_due(now), None);
        assert!(inb.members[&a].locked, "diagnostics still see its Syncs");
    }

    fn follow() -> PtpOptions {
        PtpOptions { mode: PtpMode::Follow, gptp_framing: true, pair_session: true }
    }

    #[test]
    fn follow_mode_latches_once_a_member_announced_or_after_the_sync_identity_wait() {
        let (a, b) = (addr("192.0.2.10"), addr("192.0.2.11"));
        let t0 = Instant::now();
        let mut inb = inbound(follow(), &[a, b]);
        // a Syncs without announcing: not followed yet …
        pair_from(&mut inb, a, 1, 0xA5, 1000 * S, 0, t0);
        assert_eq!((inb.latched, inb.timeline.followed()), (None, None));
        pair_from(&mut inb, a, 2, 0xA5, 1000 * S, 0, t0 + Duration::from_secs(2));
        assert_eq!(inb.latched, None);
        // … until it has Synced for FOLLOW_ANNOUNCE_WAIT: its Sync identity.
        pair_from(&mut inb, a, 3, 0xA5, 1000 * S, 0, t0 + FOLLOW_ANNOUNCE_WAIT);
        assert_eq!(inb.latched, Some(a));
        assert_eq!(inb.timeline.followed(), Some((0xA5, 1000 * S as i64)));

        // Another session: b announces first and is followed on its first pair.
        let mut inb = inbound(follow(), &[a, b]);
        pair_from(&mut inb, a, 1, 0xA5, 1000 * S, 0, t0);
        inb.on_announce(b, 0xB, 248, 248);
        pair_from(&mut inb, b, 1, 0xB5, 2000 * S, 0, t0);
        assert_eq!(inb.latched, Some(b));
        assert_eq!(inb.timeline.followed(), Some((0xB, 2000 * S as i64)), "the announced id, not the Sync identity");
        // a announcing later is noted, not followed.
        inb.on_announce(a, 0xA, 248, 248);
        pair_from(&mut inb, a, 2, 0xA5, 1000 * S, 0, t0);
        assert_eq!(inb.latched, Some(b));
        assert_eq!(inb.timeline.followed(), Some((0xB, 2000 * S as i64)));
        // Delay_Req: to the followed member, once per interval.
        assert_eq!(inb.delay_req_due(t0), Some(b));
        assert_eq!(inb.delay_req_due(t0 + Duration::from_millis(500)), None);
        assert_eq!(inb.delay_req_due(t0 + DELAY_REQ_INTERVAL), Some(b));
    }

    #[test]
    fn follow_mode_keeps_the_last_clock_after_a_stale_unlatch_until_another_member_locks() {
        let (a, b) = (addr("192.0.2.10"), addr("192.0.2.11"));
        let t0 = Instant::now();
        let mut inb = inbound(follow(), &[a, b]);
        inb.on_announce(a, 0xA, 248, 248);
        pair_from(&mut inb, a, 1, 0xA, 1000 * S, 0, t0);
        let (_, _, e1) = inb.timeline.followed_epoch().unwrap();
        inb.no_lock_logged = true;
        // b Syncs to us too while a is followed: not followed.
        inb.on_announce(b, 0xB, 248, 248);
        // a goes quiet. Not stale yet …
        inb.check_stale(t0 + FOLLOW_STALE - Duration::from_millis(1));
        assert_eq!(inb.latched, Some(a));
        // … then stale: unlatched, but its clock stays published (same
        // epoch, so no re-anchor) and the loss will be logged again.
        inb.check_stale(t0 + FOLLOW_STALE);
        assert_eq!(inb.latched, None);
        assert_eq!(inb.timeline.followed_epoch(), Some((0xA, 1000 * S as i64, e1)));
        assert!(!inb.no_lock_logged);
        assert_eq!(inb.delay_req_due(t0 + FOLLOW_STALE), None, "no Delay_Req while nobody is followed");
        // The next member whose pair arrives is followed, in one step.
        pair_from(&mut inb, b, 1, 0xB, 2000 * S, 0, t0 + FOLLOW_STALE + Duration::from_millis(100));
        assert_eq!(inb.latched, Some(b));
        let (id, off, e2) = inb.timeline.followed_epoch().unwrap();
        assert_eq!((id, off), (0xB, 2000 * S as i64));
        assert_ne!(e1, e2);
        assert_eq!(inb.delay_req_due(t0 + FOLLOW_STALE + Duration::from_millis(100)), Some(b));
    }

    #[test]
    fn follow_mode_keeps_the_old_clock_across_a_grandmaster_change_until_the_next_sample() {
        let a = addr("192.0.2.10");
        let t0 = Instant::now();
        let mut inb = inbound(follow(), &[a]);
        inb.on_announce(a, 0xA, 248, 248);
        pair_from(&mut inb, a, 1, 0xA, 1000 * S, 0, t0);
        inb.on_announce(a, 0xB, 248, 248);
        assert_eq!(inb.timeline.followed(), Some((0xA, 1000 * S as i64)));
        pair_from(&mut inb, a, 2, 0xB, 3000 * S, 0, t0);
        assert_eq!(inb.timeline.followed(), Some((0xB, 3000 * S as i64)));
        assert_eq!(inb.latched, Some(a));
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
