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
//! One master serves every receiver of a session — a HomePod stereo pair's
//! members all follow the same grandmaster, so they share one timeline.
//! In HomePod sessions packets from hosts that aren't members are ignored.
//! The ports belong to one service per local address that every session
//! joins with its own clock, so two sessions can overlap (a switch from
//! one speaker to another) and both are served from 319/320.
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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
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

/// Packets read from the general port per tick at most (a few per tick are
/// normal: Announce, Follow_Up, Signaling from each receiver).
const MAX_GENERAL_PER_TICK: usize = 64;

// libairptp's AIRPTP_INTERVAL_MS_*.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_INTERVAL: Duration = Duration::from_millis(125);
const SIGNALING_INTERVAL: Duration = Duration::from_secs(1);

/// The monotonic timeline we serve as PTP grandmaster. Receivers follow
/// this clock; the `0xD4` audio sync packets must be stamped from the
/// same instance (see [`crate::airplay::timing::spawn_sync_sender_ptp`]).
pub struct PtpMasterClock {
    start: Instant,
}

impl PtpMasterClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self { start: Instant::now() })
    }

    /// Nanoseconds on our PTP timeline (monotonic since session start).
    pub fn now_ns(&self) -> u64 {
        self.ns_at(Instant::now())
    }

    /// `at` on our PTP timeline, in nanoseconds.
    fn ns_at(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start).as_nanos() as u64
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
    /// The grandmaster identity the receiver announces (0 until seen).
    peer_clock_id: Arc<AtomicU64>,
    /// Smoothed (receiver_clock − our_clock) in ns, from its Sync/Follow_Up.
    peer_offset_ns: Arc<AtomicI64>,
    peer_locked: Arc<AtomicBool>,
    /// Anything at all arrived on the PTP ports for this session.
    peer_heard: Arc<AtomicBool>,
    /// Why the service this session joined is on temporary ports, if it is
    /// (shared with the service, so it clears when the service moves back).
    ports_problem: Arc<Mutex<Option<PortsProblem>>>,
}

impl PtpTimeline {
    pub(crate) fn new(clock: Arc<PtpMasterClock>, clock_id: u64) -> Self {
        Self {
            clock,
            clock_id,
            peer_clock_id: Arc::new(AtomicU64::new(0)),
            peer_offset_ns: Arc::new(AtomicI64::new(0)),
            peer_locked: Arc::new(AtomicBool::new(false)),
            peer_heard: Arc::new(AtomicBool::new(false)),
            ports_problem: Arc::new(Mutex::new(None)),
        }
    }

    /// Why this PC's PTP ports can't be used for this session now, if
    /// they can't: it then runs PTP from temporary ports, which receivers
    /// don't answer.
    pub fn ports_problem(&self) -> Option<PortsProblem> {
        lock(&self.ports_problem).clone()
    }

    #[cfg(test)]
    pub(crate) fn with_ports_problem(self, problem: Option<PortsProblem>) -> Self {
        *lock(&self.ports_problem) = problem;
        self
    }

    /// Whether any PTP message arrived for this session (nothing at all
    /// points at a firewall or at the ports).
    pub fn heard_from_receiver(&self) -> bool {
        self.peer_heard.load(Ordering::Acquire)
    }

    /// Whether the receiver announced a clock of its own (it masters its
    /// clock at us, the Sonos case) rather than following ours.
    pub fn receiver_announced_clock(&self) -> bool {
        self.peer_clock_id.load(Ordering::Acquire) != 0
    }

    /// Whether its Sync/Follow_Up pairs arrived, so we follow its clock.
    pub fn locked_to_receiver(&self) -> bool {
        self.peer_locked.load(Ordering::Acquire)
    }

    pub fn our_now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    /// `(receiver clock id, receiver "now" in ns)` once we've locked onto
    /// the receiver's Sync/Follow_Up stream; None before that.
    pub fn receiver_now_ns(&self) -> Option<(u64, u64)> {
        if !self.peer_locked.load(Ordering::Acquire) {
            return None;
        }
        let id = self.peer_clock_id.load(Ordering::Acquire);
        if id == 0 {
            return None;
        }
        let now = (self.clock.now_ns() as i64).checked_add(self.peer_offset_ns.load(Ordering::Acquire))?;
        u64::try_from(now).ok().map(|n| (id, n))
    }

    /// Ingest one Sync/Follow_Up pair from the receiver: `t1` = its
    /// precise origin timestamp, `t2` = our receive time. One-way path
    /// delay (sub-ms on a LAN) is absorbed into the offset — irrelevant
    /// at audio-anchor precision.
    fn note_peer_sync(&self, t1_ns: u64, t2_ns: u64) -> i64 {
        // `t1` is the receiver's, off the network: any u64.
        let sample = (t1_ns as i128 - t2_ns as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        let next = if self.peer_locked.load(Ordering::Acquire) {
            let prev = self.peer_offset_ns.load(Ordering::Acquire);
            (prev / 8) * 7 + sample / 8
        } else {
            sample
        };
        self.peer_offset_ns.store(next, Ordering::Release);
        self.peer_locked.store(true, Ordering::Release);
        next
    }

    fn set_peer_clock_id(&self, id: u64) -> u64 {
        self.peer_clock_id.swap(id, Ordering::AcqRel)
    }
}

/// One session's place in the PTP service: its own clock, served to its
/// own receivers. Dropping it (or [`PtpMaster::stop`]) leaves the service;
/// the last session on an address to leave releases the ports.
pub struct PtpMaster {
    pub timeline: PtpTimeline,
    /// UUID string for the RTSP `timingPeerInfo.ID` field.
    pub clock_uuid: String,
    shared: Arc<Shared>,
    id: u64,
}

impl PtpMaster {
    /// See [`PtpTimeline::ports_problem`].
    pub fn ports_problem(&self) -> Option<PortsProblem> {
        self.timeline.ports_problem()
    }

    /// Leave the service. When this was the last session on its address,
    /// the service stops and the ports are closed before this returns.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for PtpMaster {
    fn drop(&mut self) {
        leave(&self.shared, self.id);
    }
}

/// Start serving PTP for one session's `members` (one receiver, or each
/// member of a stereo pair) with a new clock of its own. The event (319)
/// and general (320) ports belong to one service per local address, which
/// every AirPlay 2 session on that address joins, so a session started
/// while another still runs (a switch between speakers) gets them too.
/// The new clock's first Announce, Signaling and Sync are sent before this
/// returns, so the clock is live before the RTSP SETUP that advertises it.
/// `members_only`: ignore PTP traffic from any other host (otherwise every
/// source is answered, as before).
pub fn spawn_ptp_master(
    members: &[IpAddr],
    local_ip: IpAddr,
    receiver_name: String,
    members_only: bool,
) -> Result<PtpMaster> {
    spawn_ptp_master_on(members, local_ip, PTP_PORTS, receiver_name, members_only)
}

/// The UDP ports PTP runs on: ours and the receivers'. Always
/// [`PTP_PORTS`] in the app; tests use others, because binding ports
/// below 1024 needs privileges on Linux.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PtpPorts {
    pub local_event: u16,
    pub local_general: u16,
    pub peer_event: u16,
    pub peer_general: u16,
}

pub(crate) const PTP_PORTS: PtpPorts = PtpPorts {
    local_event: PTP_EVENT_PORT,
    local_general: PTP_GENERAL_PORT,
    peer_event: PTP_EVENT_PORT,
    peer_general: PTP_GENERAL_PORT,
};

pub(crate) fn spawn_ptp_master_on(
    members: &[IpAddr],
    local_ip: IpAddr,
    ports: PtpPorts,
    receiver_name: String,
    members_only: bool,
) -> Result<PtpMaster> {
    anyhow::ensure!(!members.is_empty(), "PTP master needs at least one receiver");
    let mut rng = rand::thread_rng();
    // Keep the top bit clear: this id crosses three encodings — raw BE
    // bytes in PTP headers, a plist Integer in the SETUP `ClockID`, and
    // `networkTimeTimelineID` in SETRATEANCHORTIME. With bit 63 set, the
    // signed and unsigned plist encodings diverge (i64 cast vs u64) and a
    // strict receiver would see two different timeline ids.
    let clock_id: u64 = rng.gen::<u64>() & 0x7FFF_FFFF_FFFF_FFFF;
    let clock_uuid = format_uuid(rng.gen());
    let mut timeline = PtpTimeline::new(PtpMasterClock::new(), clock_id);
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);

    let mut registry = lock(&SERVICES);
    while registry.closing.contains(&(local_ip, ports)) {
        registry = CLOSED.wait(registry).unwrap_or_else(|e| e.into_inner());
    }
    let services = &mut registry.services;
    let index = match services.iter().position(|s| s.local_ip == local_ip && s.ports == ports) {
        Some(i) => i,
        None => {
            services.push(Service::start(local_ip, ports)?);
            services.len() - 1
        }
    };
    let shared = services[index].shared.clone();
    if lock(&shared.ports_problem).is_some() {
        shared.move_to_real_ports(local_ip);
    }
    timeline.ports_problem = shared.ports_problem.clone();
    {
        let mut sessions = lock(&shared.sessions);
        sessions.push(Registration::new(id, members.to_vec(), members_only, timeline.clone(), receiver_name));
        // A service in the registry has its sockets (see `leave`).
        if let Some(sockets) = shared.sockets() {
            transmit_due(&mut sessions, &sockets.event, &sockets.general, ports, Instant::now());
        }
    }
    drop(registry);

    info!(
        "AirPlay 2 PTP: serving as grandmaster for {:?} (clock_id={:#018x})",
        members, clock_id
    );
    Ok(PtpMaster { timeline, clock_uuid, shared, id })
}

/// The PTP ports of one local address and the thread that serves every
/// session on it.
struct Service {
    local_ip: IpAddr,
    ports: PtpPorts,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// What a service's thread shares with its sessions.
struct Shared {
    sessions: Mutex<Vec<Registration>>,
    stop: AtomicBool,
    ports: PtpPorts,
    /// The thread takes them at the start of each tick (and holds no lock
    /// while it waits on them); a joining session may move them back onto
    /// the real ports ([`Shared::move_to_real_ports`]). Replaced sockets
    /// close when the tick that still uses them ends. None once the
    /// service has stopped: `leave` closes them before it says the ports
    /// are free.
    sockets: Mutex<Option<Arc<PtpSockets>>>,
    /// Set while the service runs on temporary ports.
    ports_problem: Arc<Mutex<Option<PortsProblem>>>,
}

struct PtpSockets {
    event: UdpSocket,
    general: UdpSocket,
}

impl PtpSockets {
    fn new(event: UdpSocket, general: UdpSocket) -> Result<Self> {
        // The event port's read timeout is the service's tick.
        event.set_read_timeout(Some(Duration::from_millis(25)))?;
        general.set_nonblocking(true)?;
        Ok(Self { event, general })
    }
}

impl Shared {
    fn sockets(&self) -> Option<Arc<PtpSockets>> {
        lock(&self.sockets).clone()
    }

    /// The service is on temporary ports because the real ones couldn't be
    /// opened when it started. If both are free now, move every session's
    /// PTP onto them; otherwise stay as is.
    fn move_to_real_ports(&self, local_ip: IpAddr) {
        // A socket already on its real port is kept (it is this service's
        // own); one on a temporary port is bound again on the real one.
        let Some(current) = self.sockets() else { return };
        let real = |socket: &UdpSocket, port: u16| -> std::result::Result<UdpSocket, PortsProblem> {
            let problem = |e: std::io::Error| PortsProblem::from_bind(port, local_ip, &e);
            if socket.local_addr().map_err(&problem)?.port() == port {
                socket.try_clone().map_err(&problem)
            } else {
                UdpSocket::bind(SocketAddr::new(local_ip, port)).map_err(&problem)
            }
        };
        let moved = real(&current.event, self.ports.local_event)
            .and_then(|event| Ok((event, real(&current.general, self.ports.local_general)?)));
        drop(current);
        match moved.map(|(event, general)| PtpSockets::new(event, general)) {
            Ok(Ok(sockets)) => {
                *lock(&self.sockets) = Some(Arc::new(sockets));
                *lock(&self.ports_problem) = None;
                info!(
                    "AirPlay 2 PTP: UDP ports {} and {} on {} are free again; serving from them",
                    self.ports.local_event, self.ports.local_general, local_ip
                );
            }
            Ok(Err(e)) => debug!("AirPlay 2 PTP: could not move onto the PTP ports ({e:#})"),
            // Still not free: say which port is in the way now.
            Err(problem) => {
                debug!("AirPlay 2 PTP: the PTP ports are still not available ({problem})");
                *lock(&self.ports_problem) = Some(problem);
            }
        }
    }
}

/// Every running service, and the addresses whose service is closing.
/// Joining and leaving happen under this lock; a service's thread is
/// joined and its sockets closed outside it (so one slow stop can't hold
/// up every other start and stop), and a session starting on an address
/// whose service is closing waits on [`CLOSED`] until the ports are free,
/// so it never falls back to temporary ports because this app still held
/// them.
static SERVICES: Mutex<Registry> = Mutex::new(Registry { services: Vec::new(), closing: Vec::new() });
static CLOSED: Condvar = Condvar::new();

struct Registry {
    services: Vec<Service>,
    closing: Vec<(IpAddr, PtpPorts)>,
}
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Service {
    fn start(local_ip: IpAddr, ports: PtpPorts) -> Result<Self> {
        let (event, event_problem) = bind_ptp(local_ip, ports.local_event).context("bind PTP event :319")?;
        let (general, general_problem) = bind_ptp(local_ip, ports.local_general).context("bind PTP general :320")?;
        let shared = Arc::new(Shared {
            sessions: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            ports,
            sockets: Mutex::new(Some(Arc::new(PtpSockets::new(event, general)?))),
            ports_problem: Arc::new(Mutex::new(event_problem.or(general_problem))),
        });
        let shared_t = shared.clone();
        let thread = thread::Builder::new()
            .name(format!("stream-to-speaker-ap2-ptp:{}", local_ip))
            .spawn(move || run_service(shared_t))?;
        Ok(Self { local_ip, ports, shared, thread: Some(thread) })
    }
}

/// Remove session `id`; if it was the last one, stop its service and close
/// the ports before returning.
fn leave(shared: &Arc<Shared>, id: u64) {
    let mut registry = lock(&SERVICES);
    let last = {
        let mut sessions = lock(&shared.sessions);
        if let Some(s) = sessions.iter().find(|s| s.id == id) {
            debug!("AirPlay 2 PTP master for {} exiting ({} delay-resps served)", s.name, s.delay_resps);
        }
        sessions.retain(|s| s.id != id);
        sessions.is_empty()
    };
    if !last {
        return;
    }
    let Some(i) = registry.services.iter().position(|s| Arc::ptr_eq(&s.shared, shared)) else { return };
    let mut service = registry.services.swap_remove(i);
    let key = (service.local_ip, service.ports);
    registry.closing.push(key);
    drop(registry);
    service.shared.stop.store(true, Ordering::Release);
    if let Some(t) = service.thread.take() {
        let _ = t.join();
    }
    // Close the ports now: the last session still holds the shared state.
    lock(&service.shared.sockets).take();
    drop(service);
    lock(&SERVICES).closing.retain(|k| *k != key);
    CLOSED.notify_all();
}

/// Why a PTP port could not be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortsProblem {
    pub port: u16,
    pub ip: IpAddr,
    /// The OS said the address is in use: another program holds the port.
    pub in_use: bool,
    pub error: String,
}

impl PortsProblem {
    fn from_bind(port: u16, ip: IpAddr, e: &std::io::Error) -> Self {
        Self { port, ip, in_use: e.kind() == std::io::ErrorKind::AddrInUse, error: e.to_string() }
    }
}

impl std::fmt::Display for PortsProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.in_use {
            write!(f, "another program on this PC is using UDP port {} on {}", self.port, self.ip)
        } else {
            write!(f, "this PC could not open UDP port {} on {} ({})", self.port, self.ip, self.error)
        }
    }
}

/// Bind one PTP port. When that fails, fall back to a temporary port (the
/// session still starts, as before) and return why, for the log and the
/// session's fault message.
fn bind_ptp(local_ip: IpAddr, port: u16) -> Result<(UdpSocket, Option<PortsProblem>)> {
    match UdpSocket::bind(SocketAddr::new(local_ip, port)) {
        Ok(s) => Ok((s, None)),
        Err(e) => {
            let problem = PortsProblem::from_bind(port, local_ip, &e);
            warn!(
                "AirPlay 2 PTP: {problem}. Using a temporary port instead, which AirPlay 2 speakers \
                 don't answer, so they may not sync to this PC and stay silent."
            );
            Ok((UdpSocket::bind(SocketAddr::new(local_ip, 0))?, Some(problem)))
        }
    }
}

/// One session in a service: its clock and receivers, and the state of its
/// message exchange (what one master thread per session kept before).
struct Registration {
    id: u64,
    name: String,
    members: Vec<IpAddr>,
    members_only: bool,
    timeline: PtpTimeline,
    /// Following a receiver's own clock (the Sonos anchor nuance) only
    /// makes sense with a single receiver; several members follow us.
    follow_receiver_clock: bool,
    announce_seq: u16,
    sync_seq: u16,
    signaling_seq: u16,
    last_announce: Option<Instant>,
    last_sync: Option<Instant>,
    last_signaling: Option<Instant>,
    /// The receiver's most recent Sync (seq, our receive time) awaiting its
    /// Follow_Up — pairing them yields the receiver-clock offset.
    pending_peer_sync: Option<(u16, u64)>,
    peer_lock_logged: bool,
    delay_resps: u64,
    rx_total: u64,
    seen_types: u16,
    last_status: Instant,
}

/// The session that owns receiver `ip`: the newest one serving it. A
/// receiver hears one grandmaster at a time, also while a session that
/// lists it is being replaced.
fn owner(sessions: &[Registration], ip: IpAddr) -> Option<usize> {
    sessions.iter().rposition(|s| s.members.contains(&ip))
}

/// The session a packet from `ip` is for: its owner, else the newest
/// session that answers every host.
fn recipient(sessions: &[Registration], ip: IpAddr) -> Option<usize> {
    owner(sessions, ip).or_else(|| sessions.iter().rposition(|s| !s.members_only))
}

/// Send what is due, for every session, to the receivers it owns.
fn transmit_due(sessions: &mut [Registration], event: &UdpSocket, general: &UdpSocket, ports: PtpPorts, now: Instant) {
    for i in 0..sessions.len() {
        let owned: Vec<IpAddr> =
            sessions[i].members.iter().copied().filter(|ip| owner(sessions, *ip) == Some(i)).collect();
        sessions[i].transmit_due(&owned, event, general, ports, now);
    }
}

fn run_service(shared: Arc<Shared>) {
    let ports = shared.ports;
    let mut buf = [0u8; 256];
    while !shared.stop.load(Ordering::Acquire) {
        let Some(sockets) = shared.sockets() else { break };
        let (event, general) = (&sockets.event, &sockets.general);
        transmit_due(&mut lock(&shared.sessions), event, general, ports, Instant::now());

        // Event port: the receiver's Delay_Req arrives here. The 25 ms read
        // timeout doubles as the loop tick.
        match event.recv_from(&mut buf) {
            Ok((n, src)) => {
                let received = Instant::now();
                let mut sessions = lock(&shared.sessions);
                match recipient(&sessions, src.ip()) {
                    Some(i) => sessions[i].on_event(&buf[..n], src, received, general, ports),
                    None => debug!("AP2 PTP: ignoring packet from non-member {}", src),
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                debug!("AP2 PTP event recv error: {}", e);
            }
        }

        drain_general(general, &shared, &mut buf);
        drop(sockets);

        let now = Instant::now();
        for s in lock(&shared.sessions).iter_mut() {
            s.heartbeat(now);
        }
    }
}

/// General port: Announce / Follow_Up / Signaling / Delay_Resp, up to
/// [`MAX_GENERAL_PER_TICK`] of what is queued, so a host flooding the port
/// can't keep the thread from sending or from noticing a stop. Returns how
/// many packets were read.
fn drain_general(general: &UdpSocket, shared: &Shared, buf: &mut [u8; 256]) -> usize {
    let mut read = 0;
    while read < MAX_GENERAL_PER_TICK {
        let Ok((n, src)) = general.recv_from(buf) else { break };
        read += 1;
        let mut sessions = lock(&shared.sessions);
        match recipient(&sessions, src.ip()) {
            Some(i) => sessions[i].on_general(&buf[..n], src, shared.ports),
            None => debug!("AP2 PTP: ignoring packet from non-member {}", src),
        }
    }
    read
}

impl Registration {
    fn new(id: u64, members: Vec<IpAddr>, members_only: bool, timeline: PtpTimeline, name: String) -> Self {
        Self {
            id,
            name,
            follow_receiver_clock: members.len() == 1,
            members,
            members_only,
            timeline,
            announce_seq: 0,
            sync_seq: 0,
            signaling_seq: 0,
            last_announce: None,
            last_sync: None,
            last_signaling: None,
            pending_peer_sync: None,
            peer_lock_logged: false,
            delay_resps: 0,
            rx_total: 0,
            seen_types: 0,
            last_status: Instant::now(),
        }
    }

    fn transmit_due(&mut self, owned: &[IpAddr], event: &UdpSocket, general: &UdpSocket, ports: PtpPorts, now: Instant) {
        let clock_id = self.timeline.clock_id;
        let clock = self.timeline.clock.clone();
        if self.last_announce.map_or(true, |t| now.duration_since(t) >= ANNOUNCE_INTERVAL) {
            let pkt = build_announce(clock_id, self.announce_seq);
            for ip in owned {
                let _ = general.send_to(&pkt, SocketAddr::new(*ip, ports.peer_general));
            }
            self.announce_seq = self.announce_seq.wrapping_add(1);
            self.last_announce = Some(now);
        }

        // Apple-proprietary signaling TLVs — libairptp sends these to every
        // peer from the moment it's added (1 s cadence). Observed from iOS
        // senders; receivers appear to expect them before engaging a master.
        if self.last_signaling.map_or(true, |t| now.duration_since(t) >= SIGNALING_INTERVAL) {
            let pkt = build_signaling(clock_id, self.signaling_seq);
            for ip in owned {
                let _ = general.send_to(&pkt, SocketAddr::new(*ip, ports.peer_general));
            }
            self.signaling_seq = self.signaling_seq.wrapping_add(1);
            self.last_signaling = Some(now);
        }

        if self.last_sync.map_or(true, |t| now.duration_since(t) >= SYNC_INTERVAL) {
            // Two-step: Sync carries a coarse origin, the Follow_Up sent
            // right behind it carries the precise origin timestamp.
            // Per member, each with its own precise timestamp.
            for ip in owned {
                let coarse = clock.now_ns();
                let sync = build_sync(clock_id, self.sync_seq, coarse);
                let _ = event.send_to(&sync, SocketAddr::new(*ip, ports.peer_event));
                let precise = clock.now_ns();
                let fup = build_follow_up(clock_id, self.sync_seq, precise);
                let _ = general.send_to(&fup, SocketAddr::new(*ip, ports.peer_general));
            }
            self.sync_seq = self.sync_seq.wrapping_add(1);
            self.last_sync = Some(now);
        }
    }

    /// A packet on the event port, received at `received`. Every inbound
    /// packet is logged so the log shows whether the receiver engages our
    /// clock at all.
    fn on_event(&mut self, pkt: &[u8], src: SocketAddr, received: Instant, general: &UdpSocket, ports: PtpPorts) {
        let t_recv = self.timeline.clock.ns_at(received);
        self.rx_total += 1;
        self.timeline.peer_heard.store(true, Ordering::Release);
        let Some(h) = parse_header(pkt) else { return };
        note_inbound(&mut self.seen_types, h.msg_type, ports.local_event, src);
        match h.msg_type {
            0x1 => {
                let resp = build_delay_resp(self.timeline.clock_id, h.sequence_id, t_recv, &h.source_port_identity);
                let _ = general.send_to(&resp, SocketAddr::new(src.ip(), ports.peer_general));
                self.delay_resps += 1;
                if self.delay_resps == 1 {
                    info!("AirPlay 2 PTP: receiver {} is exchanging Delay_Req — clock lock in progress", src.ip());
                }
            }
            // The receiver's own Sync (it masters its clock at us):
            // remember (seq, our recv time) and pair it with the
            // Follow_Up's precise origin timestamp.
            0x0 if self.follow_receiver_clock => {
                self.pending_peer_sync = Some((h.sequence_id, t_recv));
            }
            _ => {}
        }
    }

    /// A packet on the general port: Announce / Follow_Up / Signaling /
    /// Delay_Resp from the receiver.
    fn on_general(&mut self, pkt: &[u8], src: SocketAddr, ports: PtpPorts) {
        self.rx_total += 1;
        self.timeline.peer_heard.store(true, Ordering::Release);
        let Some(h) = parse_header(pkt) else { return };
        note_inbound(&mut self.seen_types, h.msg_type, ports.local_general, src);
        match h.msg_type {
            0xB if self.follow_receiver_clock => {
                if let Some((gm, class)) = parse_announce_grandmaster(pkt) {
                    let prev = self.timeline.set_peer_clock_id(gm);
                    if prev != gm {
                        info!(
                            "AirPlay 2 PTP: receiver announces its own clock {:#018x} (clockClass {})",
                            gm, class
                        );
                    }
                }
            }
            // Follow_Up completing the receiver's two-step Sync: its
            // precise origin timestamp + our Sync receive time give the
            // receiver-clock offset we anchor with.
            0x8 if self.follow_receiver_clock => {
                if let Some((seq, t2)) = self.pending_peer_sync {
                    if seq == h.sequence_id {
                        if let Some(t1) = read_timestamp(pkt, HEADER_LEN) {
                            let off = self.timeline.note_peer_sync(t1, t2);
                            if !self.peer_lock_logged {
                                self.peer_lock_logged = true;
                                info!(
                                    "AirPlay 2 PTP: locked to the receiver's clock (offset {:.3} s) — can anchor on its timeline",
                                    off as f64 / 1e9
                                );
                            }
                        }
                        self.pending_peer_sync = None;
                    }
                }
            }
            _ => {}
        }
    }

    /// Every 5 s: if the receiver never talks back, the PTP clock can't
    /// lock and playback is silent — say so loudly, with the likely cause.
    fn heartbeat(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_status) < Duration::from_secs(5) {
            return;
        }
        if self.rx_total == 0 {
            warn!(
                "AirPlay 2 PTP: receiver {} has sent us NOTHING on 319/320 after 5s — it isn't \
                 engaging our clock. Likely inbound UDP 319/320 is firewall-blocked, or it \
                 expects unicast-PTP signaling we don't send yet. No clock lock = silent audio.",
                match self.members.as_slice() {
                    [one] => one.to_string(),
                    many => format!("{:?}", many),
                }
            );
        } else if self.delay_resps > 0 {
            // Delay_Resp count is only interesting when nonzero: receivers
            // that master their own clock (Sonos) never send Delay_Req, so
            // 0 is the normal state, not a failure.
            info!(
                "AirPlay 2 PTP status: {} packet(s) from receiver, {} Delay_Resp served",
                self.rx_total, self.delay_resps
            );
        } else {
            info!("AirPlay 2 PTP status: {} packet(s) from receiver", self.rx_total);
        }
        self.last_status = now;
    }
}

/// Log inbound PTP traffic: the first packet of each message type at INFO
/// (so a default log shows exactly how the receiver engages), repeats at
/// debug. `seen` is a bitmask indexed by message type.
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

/// Append one TLV: tlvType(2 BE) + lengthField(2 BE) + value bytes.
fn push_tlv(out: &mut Vec<u8>, tlv_type: u16, value: &[u8]) {
    out.extend_from_slice(&tlv_type.to_be_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
}

fn build_announce(clock_id: u64, seq: u16) -> Vec<u8> {
    // Body: originTimestamp(10) currentUtcOffset(2) reserved(1)
    // grandmasterPriority1(1) grandmasterClockQuality(4)
    // grandmasterPriority2(1) grandmasterIdentity(8) stepsRemoved(2)
    // timeSource(1) = 30 bytes, then a PATH_TRACE TLV (4+8) carrying our
    // clock identity — libairptp appends it and Apple gear expects it.
    let body_len = 30 + 4 + 8;
    let mut out = Vec::with_capacity(HEADER_LEN + body_len);
    build_header(&mut out, MSG_ANNOUNCE, FLAGS_GENERAL, clock_id, seq, body_len, 0x05, LOG_INTERVAL_ANNOUNCE);
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
fn build_signaling(clock_id: u64, seq: u16) -> Vec<u8> {
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
    build_header(&mut out, MSG_SIGNALING, FLAGS_GENERAL, clock_id, seq, body_len, 0x05, LOG_INTERVAL_SIGNALING);
    out.extend_from_slice(&[0u8; 10]); // targetPortIdentity (zeros; iOS fills it, libairptp doesn't)
    push_tlv(&mut out, TLV_ORG_EXTENSION, &tlv1);
    push_tlv(&mut out, TLV_ORG_EXTENSION, &tlv2);
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

/// Pull (grandmasterIdentity, clockClass) out of an inbound Announce.
/// Body offsets after the 34-byte header: originTimestamp(10),
/// currentUtcOffset(2), reserved(1), priority1(1), clockQuality(4),
/// priority2(1), grandmasterIdentity(8).
fn parse_announce_grandmaster(buf: &[u8]) -> Option<(u64, u8)> {
    if buf.len() < HEADER_LEN + 30 {
        return None;
    }
    let class = buf[HEADER_LEN + 14];
    let mut id = [0u8; 8];
    id.copy_from_slice(&buf[HEADER_LEN + 19..HEADER_LEN + 27]);
    Some((u64::from_be_bytes(id), class))
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
    // 48-bit seconds from the network can exceed u64 nanoseconds.
    secs.checked_mul(1_000_000_000)?.checked_add(nanos as u64)
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

    const LOCAL: &str = "127.0.0.1";

    /// Free ports for one test: ours on [`LOCAL`], the receivers' found on
    /// `peer_ip` (every receiver listens on the same pair, as with 319/320).
    fn test_ports(peer_ip: &str) -> PtpPorts {
        let (a, b) = (UdpSocket::bind((LOCAL, 0)).unwrap(), UdpSocket::bind((LOCAL, 0)).unwrap());
        let (c, d) = (UdpSocket::bind((peer_ip, 0)).unwrap(), UdpSocket::bind((peer_ip, 0)).unwrap());
        let port = |s: &UdpSocket| s.local_addr().unwrap().port();
        PtpPorts { local_event: port(&a), local_general: port(&b), peer_event: port(&c), peer_general: port(&d) }
    }

    /// A receiver on a loopback address, listening on the peer ports.
    struct FakeReceiver {
        ip: IpAddr,
        event: UdpSocket,
        general: UdpSocket,
    }

    impl FakeReceiver {
        fn bind(ip: &str, ports: PtpPorts) -> Self {
            let event = UdpSocket::bind((ip, ports.peer_event)).unwrap();
            let general = UdpSocket::bind((ip, ports.peer_general)).unwrap();
            for s in [&event, &general] {
                s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            }
            Self { ip: ip.parse().unwrap(), event, general }
        }

        /// The next message of `msg_type` on `sock`, with where it came from.
        fn next(sock: &UdpSocket, msg_type: u8) -> (Vec<u8>, SocketAddr) {
            let mut buf = [0u8; 256];
            loop {
                let (n, from) = sock.recv_from(&mut buf).expect("a PTP message");
                if parse_header(&buf[..n]).is_some_and(|h| h.msg_type == msg_type) {
                    return (buf[..n].to_vec(), from);
                }
            }
        }

        /// Discard everything already queued.
        fn drain(&self) {
            let mut buf = [0u8; 256];
            for s in [&self.event, &self.general] {
                s.set_nonblocking(true).unwrap();
                while s.recv_from(&mut buf).is_ok() {}
                s.set_nonblocking(false).unwrap();
            }
        }

        /// Send a Delay_Req and return the clock id of the Delay_Resp.
        fn delay_resp_clock(&self, ports: PtpPorts, seq: u16) -> u64 {
            self.event.send_to(&build_delay_req(0x77, seq), (LOCAL, ports.local_event)).unwrap();
            loop {
                let (resp, _) = Self::next(&self.general, MSG_DELAY_RESP);
                let h = parse_header(&resp).unwrap();
                if h.sequence_id == seq {
                    return u64::from_be_bytes(h.source_port_identity[..8].try_into().unwrap());
                }
            }
        }
    }

    /// Switching from one AirPlay 2 speaker to another starts the new
    /// session while the old one still runs. Both share this PC's PTP
    /// ports instead of the new one falling back to a temporary port its
    /// receiver never talks to; each receiver is answered on its own
    /// session's clock; the ports outlive the old session and are released
    /// with the last one.
    #[test]
    fn a_second_session_on_the_same_address_shares_the_ptp_ports() {
        let ports = test_ports("127.0.0.2");
        let local: IpAddr = LOCAL.parse().unwrap();
        let a_rx = FakeReceiver::bind("127.0.0.2", ports);
        let b_rx = FakeReceiver::bind("127.0.0.3", ports);
        let a = spawn_ptp_master_on(&[a_rx.ip], local, ports, "A".into(), false).unwrap();
        let b = spawn_ptp_master_on(&[b_rx.ip], local, ports, "B".into(), false).unwrap();
        assert_ne!(a.timeline.clock_id, b.timeline.clock_id);

        let (_, from) = FakeReceiver::next(&b_rx.event, MSG_SYNC);
        assert_eq!(from.port(), ports.local_event, "B's Sync comes from the PTP event port");
        let (announce, from) = FakeReceiver::next(&b_rx.general, MSG_ANNOUNCE);
        assert_eq!(from.port(), ports.local_general, "B's Announce comes from the PTP general port");
        assert_eq!(parse_announce_grandmaster(&announce).unwrap().0, b.timeline.clock_id);
        assert_eq!(b_rx.delay_resp_clock(ports, 0x51), b.timeline.clock_id);
        assert_eq!(a_rx.delay_resp_clock(ports, 0x52), a.timeline.clock_id);

        a.stop();
        b_rx.drain();
        let (_, from) = FakeReceiver::next(&b_rx.event, MSG_SYNC);
        assert_eq!(from.port(), ports.local_event, "B keeps the ports after A stops");
        assert_eq!(b_rx.delay_resp_clock(ports, 0x53), b.timeline.clock_id);

        assert!(b.timeline.heard_from_receiver(), "B's receiver sent a Delay_Req");
        b.stop();
        UdpSocket::bind((local, ports.local_event)).expect("event port released with the last session");
        UdpSocket::bind((local, ports.local_general)).expect("general port released with the last session");
    }

    /// A session's first messages, in the order one master thread per
    /// session sent them: Announce, Signaling, then Sync and its Follow_Up,
    /// all numbered from 0 and carrying the session's clock.
    #[test]
    fn a_session_opens_with_announce_signaling_and_sync() {
        let ports = test_ports("127.0.0.8");
        let rx = FakeReceiver::bind("127.0.0.8", ports);
        let m = spawn_ptp_master_on(&[rx.ip], LOCAL.parse().unwrap(), ports, "Solo".into(), false).unwrap();
        let id = m.timeline.clock_id.to_be_bytes();
        let mut buf = [0u8; 256];
        let mut general = Vec::new();
        for _ in 0..3 {
            let (n, _) = rx.general.recv_from(&mut buf).unwrap();
            let h = parse_header(&buf[..n]).unwrap();
            assert_eq!(h.source_port_identity[..8], id);
            general.push((h.msg_type, h.sequence_id));
        }
        assert_eq!(general, [(MSG_ANNOUNCE, 0), (MSG_SIGNALING, 0), (MSG_FOLLOW_UP, 0)]);
        let (n, _) = rx.event.recv_from(&mut buf).unwrap();
        let h = parse_header(&buf[..n]).unwrap();
        assert_eq!((h.msg_type, h.sequence_id), (MSG_SYNC, 0));
        assert_eq!(h.source_port_identity[..8], id);
        assert!(m.ports_problem().is_none());
        assert!(!m.timeline.heard_from_receiver(), "nothing came back from this receiver");
    }

    /// A stereo pair: one clock for both members; hosts that aren't
    /// members get no answer.
    #[test]
    fn a_stereo_pair_follows_one_clock_and_strangers_are_ignored() {
        let ports = test_ports("127.0.0.4");
        let (m1, m2) = (FakeReceiver::bind("127.0.0.4", ports), FakeReceiver::bind("127.0.0.5", ports));
        let stranger = FakeReceiver::bind("127.0.0.6", ports);
        let pair = spawn_ptp_master_on(&[m1.ip, m2.ip], LOCAL.parse().unwrap(), ports, "Pair".into(), true).unwrap();
        for m in [&m1, &m2] {
            let (fup, from) = FakeReceiver::next(&m.general, MSG_FOLLOW_UP);
            assert_eq!(from.port(), ports.local_general);
            assert_eq!(parse_header(&fup).unwrap().source_port_identity[..8], pair.timeline.clock_id.to_be_bytes());
            assert_eq!(m.delay_resp_clock(ports, 0x60), pair.timeline.clock_id);
        }
        stranger.event.send_to(&build_delay_req(0x99, 0x61), (LOCAL, ports.local_event)).unwrap();
        stranger.general.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut buf = [0u8; 256];
        assert!(stranger.general.recv_from(&mut buf).is_err(), "a non-member got a PTP message");
    }

    /// A receiver listed by two sessions (a pair member picked on its own
    /// while the pair still plays) hears only the newer session's clock,
    /// and the older one's again once the newer one stops.
    #[test]
    fn a_receiver_hears_only_the_newest_session_that_lists_it() {
        let ports = test_ports("127.0.0.9");
        let (m1, m2) = (FakeReceiver::bind("127.0.0.9", ports), FakeReceiver::bind("127.0.0.10", ports));
        let local: IpAddr = LOCAL.parse().unwrap();
        let pair = spawn_ptp_master_on(&[m1.ip, m2.ip], local, ports, "Pair".into(), true).unwrap();
        let solo = spawn_ptp_master_on(&[m1.ip], local, ports, "Solo".into(), true).unwrap();
        let clock_of = |rx: &FakeReceiver| {
            rx.drain();
            let (fup, _) = FakeReceiver::next(&rx.general, MSG_FOLLOW_UP);
            let mut ids = std::collections::BTreeSet::new();
            ids.insert(u64::from_be_bytes(parse_header(&fup).unwrap().source_port_identity[..8].try_into().unwrap()));
            // A second round from every session that still sends.
            std::thread::sleep(SYNC_INTERVAL * 3);
            let mut buf = [0u8; 256];
            rx.general.set_nonblocking(true).unwrap();
            while let Ok((n, _)) = rx.general.recv_from(&mut buf) {
                if let Some(h) = parse_header(&buf[..n]) {
                    ids.insert(u64::from_be_bytes(h.source_port_identity[..8].try_into().unwrap()));
                }
            }
            rx.general.set_nonblocking(false).unwrap();
            ids.into_iter().collect::<Vec<_>>()
        };
        assert_eq!(clock_of(&m1), [solo.timeline.clock_id]);
        assert_eq!(clock_of(&m2), [pair.timeline.clock_id]);
        assert_eq!(m1.delay_resp_clock(ports, 0x70), solo.timeline.clock_id);
        solo.stop();
        assert_eq!(clock_of(&m1), [pair.timeline.clock_id]);
        assert_eq!(m1.delay_resp_clock(ports, 0x71), pair.timeline.clock_id);
    }

    /// However much arrives on the general port, one tick reads a bounded
    /// number of packets, so the service still sends, and notices a stop,
    /// every tick while a host floods it.
    #[test]
    fn one_tick_reads_a_bounded_number_of_general_packets() {
        let general = UdpSocket::bind((LOCAL, 0)).unwrap();
        general.set_nonblocking(true).unwrap();
        let to = general.local_addr().unwrap();
        let flooder = UdpSocket::bind("127.0.0.14:0").unwrap();
        for i in 0..100u16 {
            flooder.send_to(&build_announce(0x99, i), to).unwrap();
        }
        thread::sleep(Duration::from_millis(50)); // loopback delivery
        let ports = PtpPorts { local_event: 0, local_general: 0, peer_event: 0, peer_general: 0 };
        let sockets = PtpSockets::new(UdpSocket::bind((LOCAL, 0)).unwrap(), UdpSocket::bind((LOCAL, 0)).unwrap()).unwrap();
        let shared = Shared {
            sessions: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            ports,
            sockets: Mutex::new(Some(Arc::new(sockets))),
            ports_problem: Arc::new(Mutex::new(None)),
        };
        let mut buf = [0u8; 256];
        assert_eq!(drain_general(&general, &shared, &mut buf), MAX_GENERAL_PER_TICK);
        assert_eq!(drain_general(&general, &shared, &mut buf), 100 - MAX_GENERAL_PER_TICK);
    }

    /// A session that starts while the last one on its address is stopping
    /// gets the PTP ports: it waits for them to be closed, never falls back
    /// to a temporary port because this app still held them.
    #[test]
    fn a_session_starting_while_the_last_one_stops_gets_the_ports() {
        let ports = test_ports("127.0.0.13");
        let local: IpAddr = LOCAL.parse().unwrap();
        let rx = FakeReceiver::bind("127.0.0.13", ports);
        for round in 0..30 {
            let a = spawn_ptp_master_on(&[rx.ip], local, ports, "A".into(), true).unwrap();
            let stopper = thread::spawn(move || a.stop());
            // Aim at the stop's last tick, while A's sockets are still open
            // (a passing run doesn't depend on hitting it).
            thread::sleep(Duration::from_millis(5));
            let b = spawn_ptp_master_on(&[rx.ip], local, ports, "B".into(), true).unwrap();
            assert!(b.ports_problem().is_none(), "round {round}: {:?}", b.ports_problem());
            stopper.join().unwrap();
            b.stop();
        }
    }

    /// A session that joins a service still on temporary ports (another
    /// program held 319 when it started) moves the service onto the real
    /// ports once they are free, for every session on it; while they are
    /// still held, everything stays on the temporary ports.
    #[test]
    fn a_joining_session_moves_the_service_back_onto_the_freed_ports() {
        let ports = test_ports("127.0.0.15");
        let local: IpAddr = LOCAL.parse().unwrap();
        let other_program = UdpSocket::bind((local, ports.local_event)).unwrap();
        let (a_rx, b_rx, c_rx) = (
            FakeReceiver::bind("127.0.0.15", ports),
            FakeReceiver::bind("127.0.0.16", ports),
            FakeReceiver::bind("127.0.0.17", ports),
        );
        let a = spawn_ptp_master_on(&[a_rx.ip], local, ports, "A".into(), true).unwrap();
        assert!(a.ports_problem().is_some());
        let b = spawn_ptp_master_on(&[b_rx.ip], local, ports, "B".into(), true).unwrap();
        assert!(b.ports_problem().is_some(), "still held: B joins the temporary ports");

        drop(other_program);
        let c = spawn_ptp_master_on(&[c_rx.ip], local, ports, "C".into(), true).unwrap();
        assert!(c.ports_problem().is_none(), "{:?}", c.ports_problem());
        assert!(a.ports_problem().is_none(), "A's service moved too");
        for rx in [&a_rx, &c_rx] {
            rx.drain();
            let (_, from) = FakeReceiver::next(&rx.event, MSG_SYNC);
            assert_eq!(from.port(), ports.local_event);
            assert_eq!(rx.delay_resp_clock(ports, 0x80), if rx.ip == a_rx.ip { a.timeline.clock_id } else { c.timeline.clock_id });
        }
        a.stop();
        b.stop();
        c.stop();
        UdpSocket::bind((local, ports.local_event)).expect("ports released with the last session");
    }

    /// After a move back that couldn't happen, the reason names the port
    /// that is taken now, not the one that was taken at the start.
    #[test]
    fn a_failed_move_names_the_port_that_is_taken_now() {
        let ports = test_ports("127.0.0.18");
        let local: IpAddr = LOCAL.parse().unwrap();
        let holds_event = UdpSocket::bind((local, ports.local_event)).unwrap();
        let holds_general = UdpSocket::bind((local, ports.local_general)).unwrap();
        let (a_rx, b_rx) = (FakeReceiver::bind("127.0.0.18", ports), FakeReceiver::bind("127.0.0.19", ports));
        let a = spawn_ptp_master_on(&[a_rx.ip], local, ports, "A".into(), true).unwrap();
        assert_eq!(a.ports_problem().map(|p| p.port), Some(ports.local_event));
        drop(holds_event);
        let b = spawn_ptp_master_on(&[b_rx.ip], local, ports, "B".into(), true).unwrap();
        let now = b.ports_problem().expect("the general port is still held");
        assert_eq!((now.port, now.in_use), (ports.local_general, true), "{now}");
        drop(holds_general);
        a.stop();
        b.stop();
    }

    /// When another program holds this PC's PTP port, the session still
    /// starts on a temporary port, and says why.
    #[test]
    fn ptp_ports_held_by_another_program_fall_back_and_say_so() {
        let ports = test_ports("127.0.0.7");
        let local: IpAddr = LOCAL.parse().unwrap();
        let _other_program = UdpSocket::bind((local, ports.local_event)).unwrap();
        let rx = FakeReceiver::bind("127.0.0.7", ports);
        let m = spawn_ptp_master_on(&[rx.ip], local, ports, "Taken".into(), false).unwrap();
        let (_, from) = FakeReceiver::next(&rx.event, MSG_SYNC);
        assert_ne!(from.port(), ports.local_event);
        let problem = m.ports_problem().expect("the session knows its PTP port is taken");
        assert_eq!((problem.port, problem.in_use), (ports.local_event, true), "{problem}");
    }

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
        let sig = build_signaling(0x1122334455667788, 9);
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
        let ann = build_announce(id, 3);
        let (gm, class) = parse_announce_grandmaster(&ann).unwrap();
        assert_eq!(gm, id);
        assert_eq!(class, 0x06);
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

    /// Timestamps come off the network: the largest a Follow_Up can carry
    /// must neither panic nor wrap into a plausible-looking time.
    #[test]
    fn hostile_receiver_timestamps_are_refused_not_wrapped() {
        let mut buf = vec![0u8; HEADER_LEN];
        buf.extend_from_slice(&[0xFF; 10]); // 2^48 - 1 s, 4.29 s of ns
        assert_eq!(read_timestamp(&buf, HEADER_LEN), None);

        for (t1, t2, offset) in [(u64::MAX, 0, i64::MAX), (0, u64::MAX, i64::MIN)] {
            let tl = PtpTimeline::new(PtpMasterClock::new(), 0x42);
            tl.set_peer_clock_id(0xABC);
            assert_eq!(tl.note_peer_sync(t1, t2), offset);
            assert_eq!(tl.receiver_now_ns(), None, "offset {offset} gives no receiver time");
            // Later samples are smoothed without overflowing either way.
            tl.note_peer_sync(t2, t1);
        }
    }

    #[test]
    fn timeline_follows_receiver_clock() {
        let tl = PtpTimeline::new(PtpMasterClock::new(), 0x42);
        assert!(tl.receiver_now_ns().is_none()); // no peer yet
        tl.set_peer_clock_id(0xABCDEF);
        assert!(tl.receiver_now_ns().is_none()); // announced but not locked
        // Receiver clock ~1000 s ahead of ours.
        let t2 = tl.our_now_ns();
        let t1 = t2 + 1_000_000_000_000;
        let off = tl.note_peer_sync(t1, t2);
        assert_eq!(off, 1_000_000_000_000);
        let (id, now) = tl.receiver_now_ns().unwrap();
        assert_eq!(id, 0xABCDEF);
        // receiver_now ≈ our_now + offset (allow scheduling slack).
        let expect = tl.our_now_ns() + 1_000_000_000_000;
        assert!((now as i64 - expect as i64).abs() < 50_000_000);
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
