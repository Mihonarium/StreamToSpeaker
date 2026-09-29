//! AirPlay 2 session: HomeKit transient pairing + encrypted RTSP setup +
//! ChaCha20-Poly1305 realtime ALAC audio + NTP timing.
//!
//! This is the AirPlay-2 sibling of [`crate::airplay::session`] (which
//! handles legacy RAOP). It targets the `_airplay._tcp` endpoint and is
//! the path used for HomePod and other AP2-only receivers.
//!
//! ## Timing
//!
//! Two timing backends, chosen by the receiver's advertised capability:
//!
//! - **NTP** (`timingProtocol: NTP`) — the classic RAOP timing/sync
//!   packets (`0x80 0xD2/0xD3` timing, `0xD4` sync), which OwnTone
//!   confirms AirPlay 2 receivers still speak. Used for receivers that
//!   don't mandate PTP.
//! - **PTP** (`timingProtocol: PTP`) — IEEE-1588 for receivers that
//!   advertise `SupportsPTP` (HomePods, Sonos). **We serve as the PTP
//!   grandmaster** ([`crate::airplay::ap2_ptp`]): the SETUP advertises our
//!   clock (`timingPeerInfo`/`timingPeerList` + `SETPEERS`), the master
//!   sends Announce/Sync/Follow_Up on UDP 319/320 and answers Delay_Req,
//!   and the `0xD4` sync packet is stamped from the same master clock
//!   (+ NTP epoch delta) so the receiver can map it onto the PTP timeline
//!   it follows. Mirrors OwnTone's `libairptp` sender design.

use anyhow::{Context, Result};
use byteorder::{BigEndian, ByteOrder};
use crossbeam_channel::{Receiver, TryRecvError};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::airplay::alac::build_uncompressed_alac_frame;
use crate::airplay::ap2_crypto::{seal_audio, ChannelCipher, TAG_LEN};
use crate::airplay::ap2_ptp::{spawn_ptp_master_multi, PtpMaster, PtpMode, PtpTimeline};
use crate::airplay::ap2_rtsp::{
    plist_brief, random_uuid, Ap2Rtsp, GroupSessionSetup, SessionTiming, StreamPorts, TransientOutcome,
};
use crate::airplay::discovery::AirPlayRenderer;
use crate::airplay::hap_pairing::PairingCredentials;
use crate::airplay::pair_experiments::{
    clock_label, group_uuid_for, order_label, ptp_options, request_order, session_rules, setpeers, ssrc_for,
    use_ptp_timing, want_buffered, ChannelMap, PairSettings, PairStream, PairTiming, SenderIdentity, Step,
};
use crate::airplay::rtp::{bind_udp, random_initial_rtptime, random_initial_seq, random_ssrc, FRAMES_PER_PACKET};
use crate::airplay::session::{mute_db, volume_pct_to_raop_db};
use crate::airplay::timing::{
    realtime_packet_duration, spawn_resend_responder, spawn_session_sync_sender, spawn_timing_responder,
    ResendBuffer, SendSchedule, SyncClock, SyncTime,
};
use crate::http_server::PcmFrame;
use crate::WIRE_SAMPLE_RATE;

/// Default AirPlay 2 RTSP port if the device didn't advertise one. Public
/// so the app's PIN-pairing ceremony dials the same port as the session.
pub const DEFAULT_AIRPLAY_PORT: u16 = 7000;
/// Receiver playback latency in samples for the sync anchor. 88200 = 2 s —
/// what iTunes actually uses with Sonos (packet-capture verified).
const DEFAULT_LATENCY_SAMPLES: u32 = 88200;
/// Below this configured anchor the buffered stream — which holds 1-2 s
/// regardless of what we ask — is skipped in favour of realtime, the
/// only AirPlay 2 stream kind that can actually deliver low latency.
const LOW_LATENCY_REALTIME_MS: u32 = 1000;
/// Recently-sent packets retained for retransmit (~4 s at 44.1 kHz).
const RESEND_BUFFER_PACKETS: usize = 512;
/// How far in the future the buffered stream's SETRATEANCHORTIME anchor is
/// placed — the receiver buffers packets until this point, absorbing
/// startup jitter.
const ANCHOR_LEAD_NS: u64 = 500_000_000;
/// Cadence of the /feedback keepalive iOS senders emit.
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// PTP follow mode: how long to wait after RECORD for a member's clock to
/// lock before the first anchor.
const FOLLOW_LOCK_WAIT: Duration = Duration::from_secs(3);

pub struct AirPlay2SessionConfig {
    /// The receiver — the session's first target.
    pub renderer: AirPlayRenderer,
    /// Further receivers played in lock-step with `renderer` (the other
    /// half of a stereo pair, the HomePods of an Apple-TV-led row). Each
    /// gets its own paired RTSP session, but all of them share ONE
    /// packetiser (identical RTP seq/timestamps), ONE PTP clock and ONE
    /// anchor; every receiver gets the full stereo stream unless the L/R
    /// split is on. Empty for a single receiver.
    pub partners: Vec<AirPlayRenderer>,
    /// Id of the speaker-list row this session was started from (a group
    /// row streams to its speakers, so it differs from `renderer`'s id).
    pub row_id: String,
    /// The row's label for a group or pair row (its `gpn`).
    pub row_label: Option<String>,
    pub local_ip: IpAddr,
    pub samples_rx: Receiver<PcmFrame>,
    pub initial_volume: Option<u32>,
    /// Skip buffered mode and use the low-latency realtime stream even on
    /// receivers that advertise buffered support (user_config experiment
    /// switch — realtime is ~250 ms vs buffered's 1-2 s, but some
    /// receivers only truly play buffered).
    pub prefer_realtime: bool,
    /// Sync-anchor latency target in ms (user_config `airplay_latency_ms`,
    /// already clamped). Drives the realtime stream's declared
    /// `latencyMin/Max` and its sync-packet anchor; below
    /// [`LOW_LATENCY_REALTIME_MS`] buffered mode is skipped outright.
    pub latency_ms: u32,
    /// Stored HomeKit persistent-pairing credentials by receiver stable
    /// id, for receivers that were PIN-paired earlier (Apple TV with
    /// access control). A receiver with an entry gets `pair-verify` with
    /// it instead of transient pairing.
    pub pairing_creds: HashMap<String, PairingCredentials>,
    /// The pair/group settings when the row is a pair/group session (a
    /// stereo pair, an Apple-TV-led row, a group led by a pair, or a pair
    /// half on its own). `None` = a single receiver: every pair/group rule
    /// is off (see [`crate::airplay::pair_experiments`] for what a single
    /// receiver sends).
    pub group: Option<GroupSessionOptions>,
    /// Config `airplay_force_gptp_framing`: gPTP framing on a
    /// single-receiver session too (pair/group sessions always use it).
    pub force_gptp_framing: bool,
}

/// What a pair/group session does differently (see
/// [`crate::airplay::pair_experiments`]).
#[derive(Debug, Clone)]
pub struct GroupSessionOptions {
    /// The settings snapshot taken at connect time.
    pub settings: PairSettings,
    /// The install's persistent sender identity, used on every connection.
    pub identity: SenderIdentity,
    /// The SETUP(session) `name` key.
    pub sender_name: String,
    /// What the row is, for the experiment log line.
    pub row_kind: String,
    /// Per member (`renderer`, then `partners`): the channel map of the
    /// L/R split. Missing entries mean full stereo.
    pub channel_maps: Vec<ChannelMap>,
    /// Per member: the IPs of its stereo-pair buddies (the apple recipe
    /// names them in SETPEERS). Missing entries mean none.
    pub buddies: Vec<Vec<IpAddr>>,
}

/// Typed outcome of [`AirPlay2Session::start`], so the caller's policy
/// decisions (launch the PIN ceremony? invalidate stored credentials?) are
/// compiler-checked matches instead of string sniffing or fragile anyhow
/// downcasts that silently break under a future `.map_err` re-wrap.
#[derive(Debug, thiserror::Error)]
pub enum Ap2StartError {
    /// The receiver refused transient pairing (HTTP 470) and no stored
    /// credentials exist: it needs a one-time PIN ceremony (Apple TV with
    /// access control / "Require Device Verification").
    #[error("{name} requires one-time PIN pairing")]
    NeedsPin {
        /// Stable id of the receiver that refused (in a multi-receiver
        /// session, not necessarily the first).
        id: String,
        name: String,
    },
    /// The receiver `id` *answered* pair-verify and rejected the stored
    /// credentials (it forgot the pairing — e.g. the user removed it on
    /// the Apple TV). The caller clears them and re-pairs; OwnTone's
    /// "key cleared + re-prompt on verify failure". Transport failures
    /// during pair-verify deliberately do NOT take this variant — they
    /// surface as [`Ap2StartError::Other`] and leave the credentials
    /// intact.
    #[error("stored pairing no longer accepted by the receiver: {source:#}")]
    VerifyRejected {
        /// Stable id of the receiver that rejected its credentials.
        id: String,
        #[source]
        source: anyhow::Error,
    },
    /// Everything else (connect failures, SETUP refusals, timeouts, …).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A live AirPlay 2 session: one receiver, or several driven in
/// lock-step (see [`AirPlay2SessionConfig::partners`]).
pub struct AirPlay2Session {
    /// The receiver, or the first of several.
    pub renderer: AirPlayRenderer,
    /// Every receiver in the session, `renderer` first.
    pub members: Vec<AirPlayRenderer>,
    /// Id of the speaker-list row the session was started from.
    pub row_id: String,
    row_label: Option<String>,
    /// One RTSP connection per member, same order as `members`.
    rtsps: Vec<Arc<Mutex<Ap2Rtsp>>>,
    /// Last volume (0..=100) pushed to the receivers; restored on unmute.
    volume_pct: AtomicU32,
    stop_flag: Arc<AtomicBool>,
    /// Set by background threads when the session has demonstrably died
    /// (audio send error, buffered TCP write failure, repeated /feedback
    /// failures). Polled by the app watchdog for auto-reconnect. Any one
    /// member dying kills the whole session: a group reconnects whole —
    /// but the member and its reason are logged first.
    health: Arc<SessionHealth>,
    /// Retransmission counters (realtime stream only; buffered has no
    /// resend path and leaves them at zero). Summed over the members.
    resend_stats: Arc<crate::airplay::timing::ResendStats>,
    sender_handle: Option<JoinHandle<()>>,
    /// Per-member timing / sync / resend / event / feedback threads.
    threads: Vec<JoinHandle<()>>,
    ptp_session: Option<PtpMaster>,
    /// (last sent seq, current rtptime) for the buffered stream — read at
    /// stop() to send the spec's FLUSHBUFFERED before TEARDOWN. None for
    /// realtime sessions.
    buffered_flush: Option<(Arc<AtomicU32>, Arc<AtomicU32>)>,
    /// Clones of the buffered data TCP streams — stop() shuts them down to
    /// unblock a sender wedged in a full-buffer write before joining it.
    data_streams: Vec<TcpStream>,
    _audio_sockets: Vec<UdpSocket>,
}

/// How long before the failure that ended a row other failure evidence
/// (an event channel the receiver closed) still counts as the row's first
/// failure. Older evidence did not end anything and is not named.
const FAILURE_EVIDENCE_WINDOW: Duration = Duration::from_secs(10);

/// Liveness of a (possibly multi-member) session. A member that fails
/// logs WHY and WHEN (relative to RECORD) before the session is marked
/// dead. Every failure is kept with the instant it was first seen — not
/// when a thread got round to declaring the member ended — so the row's
/// first failure (the reconnect watchdog repeats it) names the member
/// that actually failed first.
#[derive(Default)]
pub(crate) struct SessionHealth {
    dead: AtomicBool,
    record_at: Mutex<Option<Instant>>,
    /// `(first seen, member, what, ended the member)` for every failure so
    /// far.
    failures: Mutex<Vec<(Instant, String, String, bool)>>,
}

impl SessionHealth {
    /// RECORD has been answered by every member: failure times are
    /// reported relative to now.
    fn note_record(&self) {
        *self.record_at.lock().unwrap() = Some(Instant::now());
    }

    fn since_record_at(&self, at: Instant) -> String {
        match *self.record_at.lock().unwrap() {
            Some(r) if at >= r => format!("{:.1} s after RECORD", (at - r).as_secs_f32()),
            Some(r) => format!("{:.1} s before RECORD", (r - at).as_secs_f32()),
            None => "before RECORD".to_string(),
        }
    }

    /// Evidence that `name` failed at `at` that does not end the row by
    /// itself (the receiver closed its event channel): kept, so if the row
    /// dies its first failure can name this member and time.
    fn note_failure(&self, name: &str, reason: &str, at: Instant) {
        self.failures.lock().unwrap().push((at, name.to_string(), reason.to_string(), false));
    }

    /// `name` stopped working for `reason`, first seen at `at`: log it
    /// with that time, and mark the session dead (all-or-nothing — a pair
    /// must reconnect as a pair). The row's death is logged once, naming
    /// the earliest failure recorded by then.
    fn member_ended(&self, name: &str, reason: &str, at: Instant) {
        warn!("AP2 {} ended: {}, {}", name, reason, self.since_record_at(at));
        self.failures.lock().unwrap().push((at, name.to_string(), reason.to_string(), true));
        if !self.dead.swap(true, Ordering::AcqRel) {
            if let Some(first) = self.first_failure() {
                warn!("AP2 row dead, first failure: {}", first);
            }
        }
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// The earliest failure ("<name> <reason> (<t> s after RECORD)"), once
    /// the session is dead: the earliest member end, or evidence seen at
    /// most [`FAILURE_EVIDENCE_WINDOW`] before it.
    fn first_failure(&self) -> Option<String> {
        if !self.is_dead() {
            return None;
        }
        let first = {
            let failures = self.failures.lock().unwrap();
            let end = failures.iter().filter(|f| f.3).map(|f| f.0).min()?;
            failures.iter().filter(|f| f.0 + FAILURE_EVIDENCE_WINDOW >= end).min_by_key(|f| f.0).cloned()
        };
        first.map(|(at, name, reason, _)| format!("{} {} ({})", name, reason, self.since_record_at(at)))
    }
}

/// True when a failed RTSP request means the receiver closed or reset the
/// control connection — it never comes back.
fn rtsp_connection_closed(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .map(|io| matches!(io.kind(), UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe))
            .unwrap_or(false)
            || c.to_string().contains("connection closed")
    })
}

/// Short reason for a failed RTSP request on a member's control
/// connection: "RTSP EOF" when the receiver closed or reset it.
fn rtsp_failure_reason(e: &anyhow::Error) -> String {
    if rtsp_connection_closed(e) {
        format!("RTSP EOF ({:#})", e)
    } else {
        format!("{:#}", e)
    }
}

/// Threads spawned while a session is brought up: if the bring-up fails
/// partway, dropping the guard sets the session's stop flag (which also
/// stops threads it does not hold, like the audio sender) and joins the
/// ones it holds, so no timing responder or event channel outlives a
/// failed attempt. [`SpawnGuard::into_threads`] hands them over on
/// success.
struct SpawnGuard {
    stop_flag: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    armed: bool,
}

impl SpawnGuard {
    fn new(stop_flag: Arc<AtomicBool>) -> Self {
        Self { stop_flag, threads: Vec::new(), armed: true }
    }

    fn adopt(&mut self, handle: JoinHandle<()>) {
        self.threads.push(handle);
    }

    fn into_threads(mut self) -> Vec<JoinHandle<()>> {
        self.armed = false;
        std::mem::take(&mut self.threads)
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.stop_flag.store(true, Ordering::Release);
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}

/// One receiver's connection state while the session is brought up.
struct Half {
    renderer: AirPlayRenderer,
    rtsp: Ap2Rtsp,
    audio_key: [u8; 32],
    audio_socket: UdpSocket,
    control_socket: UdpSocket,
    /// The NTP timing responder's socket until the session takes it.
    timing_socket: Option<UdpSocket>,
    control_port: u16,
    timing_port: u16,
    /// From the timing SETUP.
    event_port: u16,
    /// From the stream SETUP.
    ports: Option<StreamPorts>,
    /// The buffered data connection (TCP), once opened.
    data: Option<TcpStream>,
}

/// Bind this receiver's sockets, connect, `GET /info`, and pair — the
/// per-receiver front half of [`AirPlay2Session::start`].
fn connect_and_pair(
    renderer: &AirPlayRenderer,
    local_ip: IpAddr,
    creds: Option<&PairingCredentials>,
    identity: Option<&SenderIdentity>,
) -> std::result::Result<Half, Ap2StartError> {
    let port = renderer.airplay_port.unwrap_or(DEFAULT_AIRPLAY_PORT);
    info!(
        "AirPlay 2: starting session to {} ({}:{})",
        renderer.friendly_name, renderer.ip, port
    );

    // UDP sockets for audio (out), control (sync out), timing (responder).
    let audio_socket = bind_udp(local_ip).context("bind AP2 audio UDP")?;
    let control_socket = bind_udp(local_ip).context("bind AP2 control UDP")?;
    let timing_socket = bind_udp(local_ip).context("bind AP2 timing UDP")?;
    let control_port = control_socket.local_addr().context("AP2 control socket addr")?.port();
    let timing_port = timing_socket.local_addr().context("AP2 timing socket addr")?.port();

    // Single receivers: a random sender identity per connection (as
    // always). Pair/group sessions: the install's persistent one.
    let mut rtsp = Ap2Rtsp::connect_as(renderer.ip, port, local_ip, Duration::from_secs(5), identity)
        .context("AirPlay 2 RTSP connect")?;

    // Canonical opener — iOS sends GET /info before pairing. Some
    // receivers initialise per-connection state on it; harmless
    // everywhere else, so failure is non-fatal.
    if let Err(e) = rtsp.get_info() {
        warn!("AirPlay 2 GET /info failed (continuing): {:#}", e);
    }

    // Pairing: a PIN-paired receiver (Apple TV with access control)
    // gets pair-verify from the stored long-term keys; everything else
    // gets transient pairing. A transient 470 means the receiver *needs*
    // PIN pairing but we have no stored keys — surface NeedsPin so the
    // app can run the one-time PIN ceremony. A pair-verify REJECTION
    // (response received, credentials refused) surfaces as
    // VerifyRejected so the app clears the stale keys; a mere transport
    // failure stays a generic error and the keys survive.
    let audio_key = if let Some(creds) = creds {
        info!("AirPlay 2: verifying stored pairing with {}", renderer.friendly_name);
        match rtsp.pair_verify(creds) {
            Ok(key) => key,
            Err(crate::airplay::ap2_rtsp::PairVerifyError::Rejected(e)) => {
                warn!(
                    "AirPlay 2: {} rejected the stored pairing ({:#}) — it will be \
                     cleared for re-pairing",
                    renderer.friendly_name, e
                );
                return Err(Ap2StartError::VerifyRejected { id: renderer.stable_id(), source: e });
            }
            Err(crate::airplay::ap2_rtsp::PairVerifyError::Transport(e)) => {
                return Err(Ap2StartError::Other(
                    e.context("AirPlay 2 HomeKit pair-verify (transport)"),
                ));
            }
        }
    } else {
        match rtsp
            .pair_setup_transient()
            .context("AirPlay 2 HomeKit transient pairing")?
        {
            TransientOutcome::Paired(key) => key,
            TransientOutcome::NeedsPin => {
                info!(
                    "AirPlay 2: {} refused transient pairing (470) — needs one-time PIN \
                     verification",
                    renderer.friendly_name
                );
                return Err(Ap2StartError::NeedsPin {
                    id: renderer.stable_id(),
                    name: renderer.friendly_name.clone(),
                });
            }
        }
    };
    info!("AirPlay 2: paired with {}", renderer.friendly_name);

    Ok(Half {
        renderer: renderer.clone(),
        rtsp,
        audio_key,
        audio_socket,
        control_socket,
        timing_socket: Some(timing_socket),
        control_port,
        timing_port,
        event_port: 0,
        ports: None,
        data: None,
    })
}

/// Label for the log lines that speak about the whole session.
fn session_label(members: &[AirPlayRenderer]) -> String {
    members
        .iter()
        .map(|r| r.friendly_name.clone())
        .collect::<Vec<_>>()
        .join(" + ")
}

/// Fallback session name: a single receiver's name; for several their
/// group name (`gpn`), else "A + B".
fn session_display_name(members: &[AirPlayRenderer]) -> String {
    match members {
        [] => String::new(),
        [one] => one.friendly_name.clone(),
        [first, ..] => first
            .group
            .group_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| session_label(members)),
    }
}

/// Negotiate the audio stream on every member: the first one with the
/// full fallback chain (buffered AAC → buffered ALAC → realtime), the
/// others with whatever the first settled on — one packetiser feeds them
/// all. Returns true for buffered.
fn negotiate_stream(
    halves: &mut [Half],
    want_buffered: bool,
    codec: &mut BufferedCodecKind,
    latency_samples: u32,
) -> Result<bool> {
    let attempt = |h: &mut Half, k: BufferedCodecKind| match k {
        BufferedCodecKind::Aac => {
            h.rtsp.setup_stream_buffered(&h.audio_key, h.control_port, 4, 1024, 0x400000, DEFAULT_LATENCY_SAMPLES)
        }
        BufferedCodecKind::Alac => {
            h.rtsp.setup_stream_buffered(&h.audio_key, h.control_port, 2, 352, 0x40000, DEFAULT_LATENCY_SAMPLES)
        }
    };
    let realtime = |h: &mut Half| h.rtsp.setup_stream(&h.audio_key, h.control_port, latency_samples);
    let buffered = {
        let first = &mut halves[0];
        let (ports, buffered) = if want_buffered {
            match attempt(first, *codec) {
                Ok(p) => {
                    info!(
                        "AirPlay 2: buffered stream accepted (type 103/{}, TCP data port {})",
                        codec.label(),
                        p.data
                    );
                    (p, true)
                }
                Err(e) if *codec == BufferedCodecKind::Aac => {
                    warn!("AirPlay 2: buffered AAC SETUP rejected ({e:#}); trying buffered ALAC");
                    *codec = BufferedCodecKind::Alac;
                    match attempt(first, *codec) {
                        Ok(p) => {
                            info!(
                                "AirPlay 2: buffered stream accepted (type 103/ALAC, TCP data port {})",
                                p.data
                            );
                            (p, true)
                        }
                        Err(e) => {
                            warn!("AirPlay 2: buffered ALAC SETUP rejected ({e:#}); falling back to realtime");
                            let p = realtime(first).context("AP2 SETUP(stream, realtime fallback)")?;
                            (p, false)
                        }
                    }
                }
                Err(e) => {
                    warn!("AirPlay 2: buffered SETUP rejected ({e:#}); falling back to realtime");
                    let p = realtime(first).context("AP2 SETUP(stream, realtime fallback)")?;
                    (p, false)
                }
            }
        } else {
            let p = realtime(first).context("AP2 SETUP(stream)")?;
            (p, false)
        };
        first.ports = Some(ports);
        buffered
    };
    let codec = *codec;
    for h in halves.iter_mut().skip(1) {
        // Same kind and codec as the first receiver, or they can't share
        // one packetiser — no per-receiver fallback.
        let p = if buffered { attempt(h, codec) } else { realtime(h) }.with_context(|| {
            format!(
                "AP2 SETUP(stream {}) on {} — every receiver played together must accept the \
                 stream kind the first one negotiated",
                if buffered { format!("buffered/{}", codec.label()) } else { "realtime".to_string() },
                h.renderer.friendly_name
            )
        })?;
        h.ports = Some(p);
    }
    for h in halves.iter() {
        let ports = h.ports.as_ref().expect("every half negotiated a stream");
        debug!(
            "AirPlay 2: {} data port {}, control port {}",
            h.renderer.friendly_name, ports.data, ports.control
        );
    }
    Ok(buffered)
}

/// The distinct channel maps of a session's members (first-seen order)
/// and, per member, the index of its map — one payload is built per
/// distinct map and shared by the members that use it.
fn distinct_maps(member_maps: &[ChannelMap]) -> (Vec<ChannelMap>, Vec<usize>) {
    let mut maps: Vec<ChannelMap> = Vec::new();
    let index = member_maps
        .iter()
        .map(|m| match maps.iter().position(|x| x == m) {
            Some(i) => i,
            None => {
                maps.push(*m);
                maps.len() - 1
            }
        })
        .collect();
    if maps.is_empty() {
        maps.push(ChannelMap::Stereo);
    }
    (maps, index)
}

impl AirPlay2Session {
    pub fn start(cfg: AirPlay2SessionConfig) -> std::result::Result<Self, Ap2StartError> {
        let members: Vec<AirPlayRenderer> = std::iter::once(cfg.renderer.clone())
            .chain(cfg.partners.iter().cloned())
            .collect();
        let paired = members.len() > 1;
        let label = session_label(&members);
        let group = cfg.group.as_ref();
        let recipe = group.map(|g| g.settings.recipe);

        // Timing-protocol choice: receivers advertising SupportsPTP (bit 41)
        // get the full PTP path — field-tested on a SYMFONISK whose current
        // firmware stalls the stream SETUP under timingProtocol=NTP (NTP
        // appears as vestigial as its RAOP). For PTP **we are the
        // grandmaster** (a one-member session, and single receivers, also
        // follow the receiver's own clock once it announces one). A pair
        // shares the one master (it owns UDP 319/320) and must agree on the
        // protocol. A pair/group session set to `ntp` uses NTP. Decided
        // before connecting: a pair/group session on NTP starts its timing
        // responders before the first SETUP advertises their ports.
        let pair_timing = group.map(|g| g.settings.timing);
        let use_ptp = use_ptp_timing(pair_timing, cfg.renderer.expects_ptp());
        if pair_timing != Some(PairTiming::Ntp) {
            if let Some(odd) = members.iter().find(|r| r.expects_ptp() != cfg.renderer.expects_ptp()) {
                return Err(anyhow::anyhow!(
                    "{} and {} disagree on PTP timing; receivers played together need one shared clock",
                    cfg.renderer.friendly_name,
                    odd.friendly_name
                )
                .into());
            }
        }
        let ptp_opts = ptp_options(group.map(|g| g.settings.ptp_role), members.len(), cfg.force_gptp_framing);
        // Session-level rules: all off for a single receiver.
        let rules = session_rules(group.is_some(), members.len());
        // Request order: a single receiver's own; a pair/group session's
        // recipe on PTP, pyatv's on NTP.
        let order = request_order(recipe, use_ptp);

        // Stream kind: buffered (type 103, TCP — what iOS actually uses,
        // and seemingly the only kind current Sonos firmware truly plays)
        // when the receiver advertises bit 40 and we're on PTP; realtime
        // (type 96, UDP) otherwise. Pair/group sessions default to
        // realtime (what Music Assistant and OwnTone use). Codec: AAC-LC
        // via the Windows-provided Media Foundation encoder — iOS's
        // buffered codec, the only one field-proven on Sonos — with ALAC
        // as fallback (no encoder needed) and realtime as the last
        // resort. Every rejection is visible. One packetiser
        // feeds every member, so the kind and codec are decided once: the
        // first member negotiates with the full fallback chain, the others
        // must accept the same.
        let latency_samples = crate::airplay::timing::latency_ms_to_samples(cfg.latency_ms);
        let low_latency = cfg.latency_ms < LOW_LATENCY_REALTIME_MS;
        let all_buffered = members.iter().all(|r| r.supports_buffered_audio());
        let pair_stream = group.map(|g| g.settings.stream);
        let want_buffered = want_buffered(pair_stream, use_ptp, all_buffered, cfg.prefer_realtime, low_latency);

        // The experiment line: every switch this session runs with, with
        // the clock behaviour and request order it actually runs.
        match group {
            Some(g) => {
                let who: Vec<String> =
                    members.iter().map(|r| format!("{} {}", r.friendly_name, r.ip)).collect();
                info!(
                    "{}",
                    g.settings.experiment_line(
                        &g.row_kind,
                        &who,
                        clock_label(use_ptp.then_some(ptp_opts)),
                        &order_label(order, use_ptp, want_buffered),
                    )
                );
            }
            None => info!(
                "AP2 experiment: off — {} is a single receiver (pair/group settings do not apply)",
                label
            ),
        }
        if paired {
            info!(
                "AirPlay 2: {} receivers {} — one session each, one shared clock and anchor",
                members.len(),
                label
            );
        }

        // Every thread spawned during the bring-up is stopped (and joined)
        // if it fails partway.
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut guard = SpawnGuard::new(stop_flag.clone());
        let health = Arc::new(SessionHealth::default());
        let resend_stats = Arc::new(crate::airplay::timing::ResendStats::default());

        // Connect + pair every member first: the session either comes up
        // whole or not at all (an Ap2Rtsp that paired tears itself down
        // on drop, so an early return leaves no half-open sessions). On
        // NTP a pair/group session starts each member's timing responder
        // right after pairing, before SETUP(session) advertises its port,
        // as pyatv, OwnTone and our RAOP path do. A single receiver starts
        // it after RECORD, as it always has.
        let identity = group.map(|g| &g.identity);
        let mut halves: Vec<Half> = Vec::with_capacity(members.len());
        for r in &members {
            let mut h = connect_and_pair(r, cfg.local_ip, cfg.pairing_creds.get(&r.stable_id()), identity)?;
            if use_ptp {
                // The NTP timing responder is unused under PTP; release its socket.
                drop(h.timing_socket.take());
            } else if rules.early_ntp_responder {
                let timing_socket = h.timing_socket.take().expect("a fresh half owns its timing socket");
                guard.adopt(
                    spawn_timing_responder(timing_socket, stop_flag.clone(), r.friendly_name.clone())
                        .context("spawning AP2 timing responder")?,
                );
            }
            halves.push(h);
        }

        info!(
            "AirPlay 2: timing for {} = {} (model {:?}){}",
            label,
            match (use_ptp, ptp_opts.mode) {
                (false, _) => "NTP",
                (true, PtpMode::Follow) => "PTP (we follow a member's clock)",
                (true, PtpMode::Single) if members.len() == 1 => {
                    "PTP (we serve as grandmaster and follow the receiver's own clock once it announces one)"
                }
                (true, _) => "PTP (we serve as grandmaster)",
            },
            cfg.renderer.model.as_deref().unwrap_or("?"),
            if !use_ptp && cfg.renderer.expects_ptp() { " — forced by the pair/group timing setting" } else { "" },
        );
        // Start the master before the SETUP that advertises it, so its
        // clock identity goes into the SETUP payload and the clock (plus
        // Announce/Signaling) is already being served when the receiver
        // processes it.
        let ptp_session = if use_ptp {
            let ips: Vec<IpAddr> = members.iter().map(|r| r.ip).collect();
            Some(
                spawn_ptp_master_multi(ips, cfg.local_ip, label.clone(), ptp_opts)
                    .context("starting AP2 PTP master")?,
            )
        } else {
            None
        };

        if pair_stream == Some(PairStream::Realtime) {
            info!("AirPlay 2: pair/group stream setting = realtime — using the realtime stream");
        } else if cfg.prefer_realtime {
            info!("AirPlay 2: prefer_realtime_airplay set — using the realtime stream");
        } else if low_latency && use_ptp && all_buffered {
            info!(
                "AirPlay 2: airplay_latency_ms={} is below {} ms — using the realtime stream \
                 (buffered holds seconds regardless)",
                cfg.latency_ms, LOW_LATENCY_REALTIME_MS
            );
        }
        let mut codec = BufferedCodecKind::Alac;
        #[cfg(windows)]
        if want_buffered {
            match crate::airplay::aac_mf::AacEncoder::new() {
                Ok(_) => codec = BufferedCodecKind::Aac,
                Err(e) => {
                    warn!("AirPlay 2: Windows AAC encoder unavailable ({e:#}); buffered will use ALAC")
                }
            }
        }

        // The bring-up, in the order decided above. A single receiver
        // keeps the order it always had: SETUP(session), event channel,
        // SETUP(stream), data connection, SETPEERS, RECORD, volume. The
        // session's correlation UUID is our clock's UUID on PTP (the apple
        // recipe sends it as the timing peer's ID too; see
        // `group_session_dict`).
        let session_group_uuid = random_uuid();
        let correlation_uuid = match &ptp_session {
            Some(p) => p.clock_uuid.clone(),
            None => random_uuid(),
        };
        let mut data_streams: Vec<TcpStream> = Vec::new();
        let mut buffered: Option<bool> = None;
        for step in order {
            match step {
                Step::SetupSession => {
                    for h in &mut halves {
                        let name = h.renderer.friendly_name.clone();
                        let ts = match (group, &ptp_session) {
                            (Some(g), ptp) => {
                                let setup = GroupSessionSetup {
                                    recipe: g.settings.recipe,
                                    timing: match ptp {
                                        Some(p) => SessionTiming::Ptp {
                                            clock_id: p.timeline.clock_id,
                                            clock_uuid: p.clock_uuid.clone(),
                                        },
                                        None => SessionTiming::Ntp { timing_port: h.timing_port },
                                    },
                                    name: g.sender_name.clone(),
                                    group_uuid: group_uuid_for(g.settings.recipe, &session_group_uuid, random_uuid),
                                    correlation_uuid: correlation_uuid.clone(),
                                    sender_relay: g.settings.sender_relay,
                                };
                                h.rtsp
                                    .setup_session_group(&setup)
                                    .with_context(|| format!("AP2 SETUP(session) on {}", name))?
                            }
                            (None, Some(ptp)) => h
                                .rtsp
                                .setup_timing_ptp(ptp.timeline.clock_id, &ptp.clock_uuid)
                                .with_context(|| format!("AP2 SETUP(timing/PTP) on {}", name))?,
                            (None, None) => h
                                .rtsp
                                .setup_timing_ntp(h.timing_port)
                                .with_context(|| format!("AP2 SETUP(timing/NTP) on {}", name))?,
                        };
                        // What we sent and everything the receiver said
                        // (timingPeerInfo / TightSyncUUID / ClockID live here).
                        info!(
                            "AP2 SETUP(session) {}: sent keys [{}]; reply {}",
                            name,
                            ts.sent_keys.join(", "),
                            ts.reply.as_ref().map(plist_brief).unwrap_or_else(|| "<empty or not a plist>".into())
                        );
                        h.event_port = ts.event_port;
                    }
                }
                Step::Events => {
                    // The receiver withholds its RECORD response until the
                    // sender has a TCP connection to its eventPort. A single
                    // receiver's channel is only kept open (and drained);
                    // pair/group sessions decrypt the receiver's requests,
                    // log them and answer 200 (OwnTone airplay_events.c).
                    for h in &mut halves {
                        let ciphers = if group.is_some() { h.rtsp.take_event_ciphers() } else { None };
                        if let Some(t) = spawn_event_channel(
                            h.renderer.ip,
                            h.event_port,
                            stop_flag.clone(),
                            h.renderer.friendly_name.clone(),
                            ciphers,
                            health.clone(),
                        ) {
                            guard.adopt(t);
                        }
                    }
                }
                Step::SetupStream => {
                    buffered = Some(negotiate_stream(&mut halves, want_buffered, &mut codec, latency_samples)?);
                }
                Step::DataConnection => {
                    // Open the buffered data connections straight after the
                    // stream SETUP. Receivers hold connection state per phase
                    // (the event channel must exist before RECORD, the anchor
                    // only works at first audio), and real senders connect
                    // the data socket early too.
                    if buffered == Some(true) {
                        for h in &mut halves {
                            let data_addr = SocketAddr::new(h.renderer.ip, h.ports.as_ref().unwrap().data);
                            let stream = TcpStream::connect_timeout(&data_addr, Duration::from_secs(3))
                                .with_context(|| format!("connecting AP2 buffered data TCP to {}", data_addr))?;
                            stream.set_nodelay(true).ok();
                            // Without a write timeout, a receiver that stops
                            // consuming fills the socket buffer and write_all
                            // blocks forever — which wedges stop() on the
                            // sender join and leaves the whole app stuck
                            // "Connecting…" with TEARDOWN never sent.
                            stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
                            data_streams.extend(stream.try_clone().ok());
                            h.data = Some(stream);
                        }
                    }
                }
                Step::SetPeers => {
                    // Hand each receiver its PTP peer address list. Single
                    // receiver and `ma`: itself, then the sender (OwnTone's
                    // and airplay-cli's per-session list). `apple`: itself,
                    // its pair buddies, then the sender, as
                    // `/peer-list-changed`.
                    if use_ptp {
                        for (i, h) in halves.iter_mut().enumerate() {
                            let buddies: &[IpAddr] =
                                group.and_then(|g| g.buddies.get(i)).map(Vec::as_slice).unwrap_or(&[]);
                            let (list, content_type) = setpeers(recipe, h.renderer.ip, buddies, cfg.local_ip);
                            if group.is_some() {
                                info!(
                                    "AP2 SETPEERS {}: [{}] as {}",
                                    h.renderer.friendly_name,
                                    list.iter().map(|ip| ip.to_string()).collect::<Vec<_>>().join(", "),
                                    content_type
                                );
                            }
                            if let Err(e) = h.rtsp.set_peers_typed(&list, content_type) {
                                warn!(
                                    "AirPlay 2 SETPEERS on {} failed (PTP may not lock): {}",
                                    h.renderer.friendly_name, e
                                );
                            }
                        }
                    }
                }
                Step::Record => {
                    for h in &mut halves {
                        h.rtsp
                            .record()
                            .with_context(|| format!("AP2 RECORD on {}", h.renderer.friendly_name))?;
                    }
                    health.note_record();
                }
                Step::Volume => {
                    if let Some(vol) = cfg.initial_volume {
                        for h in &mut halves {
                            if let Err(e) = h.rtsp.set_volume(volume_pct_to_raop_db(vol)) {
                                warn!("AirPlay 2 initial volume on {} failed: {}", h.renderer.friendly_name, e);
                            }
                        }
                    }
                }
            }
        }
        let buffered = buffered.expect("every request order negotiates the stream");

        // Follow mode: give the members a moment to Sync to us so the very
        // first anchor / sync packet is already on the followed clock.
        let follow = use_ptp && ptp_opts.mode == PtpMode::Follow;
        if let (Some(ptp), true) = (&ptp_session, follow) {
            let t0 = Instant::now();
            while ptp.timeline.receiver_now_ns().is_none() && t0.elapsed() < FOLLOW_LOCK_WAIT {
                std::thread::sleep(Duration::from_millis(50));
            }
            match ptp.timeline.receiver_now_ns() {
                Some((id, _)) => info!(
                    "AirPlay 2 PTP follow: anchoring on GM {:#018x} (locked {} ms after RECORD)",
                    id,
                    t0.elapsed().as_millis()
                ),
                None if buffered => warn!(
                    "AirPlay 2 PTP follow: no member's clock followed within {} s — the buffered anchor \
                     waits until one is (it is never put on our own clock, which follow mode does not serve)",
                    FOLLOW_LOCK_WAIT.as_secs()
                ),
                None => warn!(
                    "AirPlay 2 PTP follow: no member's clock followed within {} s — realtime sync packets \
                     use our own clock (which follow mode does not serve) until one is",
                    FOLLOW_LOCK_WAIT.as_secs()
                ),
            }
        }

        // The L/R split: one payload per distinct channel map, shared by
        // every member with that map (one RTP timeline either way).
        let member_maps: Vec<ChannelMap> = (0..halves.len())
            .map(|i| group.and_then(|g| g.channel_maps.get(i).copied()).unwrap_or(ChannelMap::Stereo))
            .collect();
        let (maps, payload_index) = distinct_maps(&member_maps);
        if maps.iter().any(|m| *m != ChannelMap::Stereo) {
            info!(
                "AirPlay 2: L/R split — {}",
                halves
                    .iter()
                    .zip(&member_maps)
                    .map(|(h, m)| format!("{} gets {}", h.renderer.friendly_name, m.label()))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // From here the RTSP connections are shared: the buffered sender
        // anchors on them at first audio, the feedback keepalives post to
        // them, and volume changes arrive from arbitrary threads.
        let mut rtsps: Vec<Arc<Mutex<Ap2Rtsp>>> = Vec::with_capacity(halves.len());
        let mut audio_sockets: Vec<UdpSocket> = Vec::with_capacity(halves.len());
        let mut outlets: Vec<Outlet> = Vec::with_capacity(halves.len());
        let mut controls: Vec<(UdpSocket, SocketAddr, String)> = Vec::with_capacity(halves.len());
        // Single receiver on NTP: the timing responder starts after RECORD.
        let mut late_timing: Vec<(UdpSocket, String)> = Vec::new();
        for (i, h) in halves.into_iter().enumerate() {
            late_timing.extend(h.timing_socket.map(|t| (t, h.renderer.friendly_name.clone())));
            let ports = h.ports.expect("every half negotiated a stream");
            rtsps.push(Arc::new(Mutex::new(h.rtsp)));
            controls.push((h.control_socket, SocketAddr::new(h.renderer.ip, ports.control), h.renderer.friendly_name.clone()));
            let dest = match h.data {
                Some(stream) => OutletDest::Tcp(stream),
                None => OutletDest::Udp {
                    socket: h.audio_socket.try_clone().context("clone AP2 audio socket")?,
                    addr: SocketAddr::new(h.renderer.ip, ports.data),
                },
            };
            outlets.push(Outlet {
                name: h.renderer.friendly_name.clone(),
                audio_key: h.audio_key,
                dest,
                resend: (!buffered).then(|| ResendBuffer::new(RESEND_BUFFER_PACKETS)),
                payload: payload_index[i],
            });
            audio_sockets.push(h.audio_socket);
        }

        // Background threads. One RTP timeline for every member: the same
        // seq/rtptime on each half is what keeps a pair in lock-step.
        let initial_seq = random_initial_seq();
        let initial_rtptime = random_initial_rtptime();
        let ssrc = ssrc_for(rules.shared_timeline, use_ptp, buffered, random_ssrc);
        if rules.shared_timeline && ssrc == 0 {
            info!("AirPlay 2: RTP SSRC 0 (realtime on PTP, as Music Assistant and OwnTone send pairs)");
        }
        let current_rtptime = Arc::new(AtomicU32::new(initial_rtptime));

        let mut buffered_flush = None;
        let sender_handle = if buffered {
            // Buffered: audio goes over the (already-connected) TCP data
            // connections; playback is anchored by SETRATEANCHORTIME. No
            // sync packets, no resend (TCP is reliable), no NTP timing
            // responder (buffered implies PTP).
            drop(late_timing);
            let last_seq = Arc::new(AtomicU32::new(initial_seq as u32));
            buffered_flush = Some((last_seq.clone(), current_rtptime.clone()));
            // The anchor (SETRATEANCHORTIME) is sent by the sender thread
            // itself, right before the first real audio packet — anchoring
            // earlier maps an rtpTime to a wall instant that has passed by
            // the time audio exists, and the receiver drops everything as
            // late. Field-tested: this receiver accepts anchors only on
            // ITS OWN timeline, which the PTP layer follows via the
            // receiver's Sync/Follow_Up stream.
            let sender = spawn_ap2_buffered_sender(BufferedSenderConfig {
                outlets,
                maps,
                initial_seq,
                initial_rtptime,
                ssrc,
                samples_rx: cfg.samples_rx,
                stop_flag: stop_flag.clone(),
                receiver_name: label.clone(),
                current_rtptime: current_rtptime.clone(),
                last_seq,
                rtsps: rtsps.clone(),
                timeline: ptp_session.as_ref().unwrap().timeline.clone(),
                codec,
                health: health.clone(),
                follow,
            })?;
            info!(
                "AirPlay 2: buffered {} stream armed — will anchor at first audio",
                codec.label()
            );
            drop(controls);
            sender
        } else {
            // Anchor before audio: the sync sender sends its initial
            // (extension-bit) sync to every member synchronously before
            // returning, so spawning it BEFORE the audio sender guarantees
            // every receiver has a timeline anchor before the first audio
            // packet arrives — same ordering rule as the RAOP path. ONE
            // sync sender for the whole session: every member gets the same
            // packet each tick. Two or more members sharing one timeline
            // stamp it from the sender's schedule (the exact send time of
            // the rtptime it names); a single receiver and a one-member
            // session sample the clock when the sync thread wakes.
            for (timing_socket, name) in late_timing {
                guard.adopt(
                    spawn_timing_responder(timing_socket, stop_flag.clone(), name)
                        .context("spawning AP2 timing responder")?,
                );
            }
            let schedule = rules.shared_timeline.then(|| SendSchedule::new(initial_rtptime));
            let sync_time = match &schedule {
                Some(s) => SyncTime::Scheduled(s.clone()),
                None => SyncTime::Sampled(current_rtptime.clone()),
            };
            let sync_clock = match &ptp_session {
                Some(ptp) => SyncClock::Ptp(ptp.timeline.clone()),
                None => SyncClock::Ntp,
            };
            let mut sync_dests: Vec<(UdpSocket, SocketAddr)> = Vec::with_capacity(controls.len());
            let mut resend_controls: Vec<(UdpSocket, SocketAddr, String)> = Vec::with_capacity(controls.len());
            for (control_socket, sync_addr, name) in controls {
                // The control socket carries outbound sync packets and
                // inbound resend requests — clone it so both threads can
                // use it.
                let control_for_resend = control_socket.try_clone().context("clone AP2 control socket")?;
                sync_dests.push((control_socket, sync_addr));
                resend_controls.push((control_for_resend, sync_addr, name));
            }
            guard.adopt(
                spawn_session_sync_sender(
                    sync_dests,
                    latency_samples,
                    sync_clock,
                    sync_time,
                    stop_flag.clone(),
                    label.clone(),
                )
                .context("spawning AP2 sync sender")?,
            );

            let resends: Vec<Arc<ResendBuffer>> =
                outlets.iter().map(|o| o.resend.clone().expect("realtime outlets keep a resend buffer")).collect();
            let sender = spawn_ap2_sender(Ap2SenderConfig {
                outlets,
                maps,
                initial_seq,
                initial_rtptime,
                ssrc,
                samples_rx: cfg.samples_rx,
                stop_flag: stop_flag.clone(),
                receiver_name: label.clone(),
                current_rtptime,
                health: health.clone(),
                schedule,
            })?;

            for ((control_for_resend, sync_addr, name), resend) in resend_controls.into_iter().zip(resends) {
                guard.adopt(
                    spawn_resend_responder(
                        control_for_resend,
                        sync_addr,
                        resend,
                        resend_stats.clone(),
                        stop_flag.clone(),
                        name,
                    )
                    .context("spawning AP2 resend responder")?,
                );
            }
            sender
        };

        info!(
            "AirPlay 2: session up — {} ({} audio)",
            label,
            if buffered { "buffered/TCP" } else { "realtime/UDP" },
        );

        // /feedback keepalive — iOS senders POST this every ~2 s; some
        // receivers eventually drop (or never fully start) sessions
        // without it. Shares the RTSP connection via the session mutex.
        for (rtsp, r) in rtsps.iter().zip(&members) {
            if let Some(t) = spawn_feedback_keepalive(
                rtsp.clone(),
                stop_flag.clone(),
                health.clone(),
                r.friendly_name.clone(),
                rules.end_on_first_eof,
            ) {
                guard.adopt(t);
            }
        }

        Ok(Self {
            renderer: cfg.renderer,
            members,
            row_id: cfg.row_id,
            row_label: cfg.row_label,
            rtsps,
            volume_pct: AtomicU32::new(cfg.initial_volume.unwrap_or(100)),
            stop_flag,
            health,
            resend_stats,
            sender_handle: Some(sender_handle),
            threads: guard.into_threads(),
            ptp_session,
            buffered_flush,
            data_streams,
            _audio_sockets: audio_sockets,
        })
    }

    /// True once a background thread flagged the session dead (dropped
    /// receiver). Polled by the app watchdog for auto-reconnect.
    pub fn is_dead(&self) -> bool {
        self.health.is_dead()
    }

    /// The first member failure that killed the session ("<name> <reason>
    /// (<t> s after RECORD)"), once it is dead.
    pub fn first_failure(&self) -> Option<String> {
        self.health.first_failure()
    }

    /// `(resend requests, packets re-sent)` so far this session.
    pub fn resend_stats(&self) -> (u64, u64) {
        self.resend_stats.snapshot()
    }

    /// What the UI calls this session: the group or pair row's name when
    /// started from one, else the receiver's name (or its `gpn`, else
    /// "A + B", for several).
    pub fn display_name(&self) -> String {
        self.row_label.clone().unwrap_or_else(|| session_display_name(&self.members))
    }

    pub fn set_volume_pct(&self, vol: u32) -> Result<()> {
        self.volume_pct.store(vol.min(100), Ordering::Relaxed);
        self.for_each_rtsp(|rtsp| rtsp.set_volume(volume_pct_to_raop_db(vol)))
    }

    pub fn set_mute(&self, muted: bool) -> Result<()> {
        let db = mute_db(muted, self.volume_pct.load(Ordering::Relaxed));
        self.for_each_rtsp(|rtsp| rtsp.set_volume(db))
    }

    /// Run `f` on every member's RTSP connection; every member is tried,
    /// the first failure is returned.
    fn for_each_rtsp(&self, f: impl Fn(&mut Ap2Rtsp) -> Result<()>) -> Result<()> {
        let mut first_err = None;
        for (rtsp, r) in self.rtsps.iter().zip(&self.members) {
            if let Err(e) = f(&mut rtsp.lock().unwrap()) {
                let e = e.context(format!("on {}", r.friendly_name));
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    pub fn stop(mut self) {
        info!("AirPlay 2: stopping session to {}", self.display_name());
        self.stop_flag.store(true, Ordering::Release);
        // Unblock a buffered sender wedged in a full-buffer TCP write —
        // without this the join below can hang forever on a receiver
        // that stopped consuming, freezing the whole switch-speaker flow.
        for s in &self.data_streams {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        for h in self.sender_handle.take().into_iter().chain(self.threads.drain(..)) {
            let _ = h.join();
        }
        if let Some(ptp) = self.ptp_session.take() {
            ptp.stop();
        }
        // Buffered sessions get the spec's FLUSHBUFFERED before TEARDOWN
        // so the receiver drops its buffered tail instead of playing it out.
        let flush = self.buffered_flush.take();
        for rtsp in &self.rtsps {
            let mut guard = rtsp.lock().unwrap();
            if let Some((seq, ts)) = &flush {
                if let Err(e) = guard.flush_buffered(seq.load(Ordering::Acquire), ts.load(Ordering::Acquire)) {
                    debug!("AirPlay 2 FLUSHBUFFERED failed (continuing to TEARDOWN): {:#}", e);
                }
            }
            guard.teardown();
        }
    }
}

impl Drop for AirPlay2Session {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
    }
}

/// Seal one RTP packet for every outlet: the header is the members'
/// shared timeline, the payload is the one for the outlet's channel map
/// (`payloads[outlet.payload]` — one shared payload unless the L/R split
/// is on), the ChaCha seal is per member key. Result order = outlet order.
fn seal_for_outlets(outlets: &[Outlet], header: &[u8; 12], seq: u16, payloads: &[Vec<u8>]) -> Vec<Vec<u8>> {
    outlets
        .iter()
        .map(|o| {
            let sealed = seal_audio(&o.audio_key, header, seq, &payloads[o.payload]);
            let mut packet = Vec::with_capacity(12 + sealed.len());
            packet.extend_from_slice(header);
            packet.extend_from_slice(&sealed);
            packet
        })
        .collect()
}

/// Where one member's sealed packets go.
enum OutletDest {
    /// Realtime stream: UDP to the receiver's data port.
    Udp { socket: UdpSocket, addr: SocketAddr },
    /// Buffered stream: the length-framed TCP data connection.
    Tcp(TcpStream),
}

/// One member of the session as the packetiser sees it: its own audio
/// key (pairing is per connection, so the seal differs per member) and
/// its own transport. The RTP header and payload are shared.
struct Outlet {
    name: String,
    audio_key: [u8; 32],
    dest: OutletDest,
    /// Realtime only: this member's retransmit ring (sealed packets are
    /// per key, so the ring is per member).
    resend: Option<Arc<ResendBuffer>>,
    /// Index of this member's channel map in the sender's `maps` (always
    /// 0 unless the L/R split is on).
    payload: usize,
}

/// Open the AirPlay 2 event channel — a TCP connection to the receiver's
/// `eventPort` (from the first SETUP response). The receiver withholds its
/// RECORD response until this connection exists. Best-effort: a
/// missing/unreachable port logs and returns None rather than failing the
/// session.
///
/// Without `ciphers` (single receivers) a drain thread keeps the socket
/// open and logs what arrives, undecoded. With them (pair/group sessions)
/// the receiver's requests are decrypted with the pairing's event keys,
/// logged (method, URI, plist) and answered `200 OK`, as OwnTone's
/// airplay_events.c does.
fn spawn_event_channel(
    receiver_ip: IpAddr,
    event_port: u16,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
    ciphers: Option<(ChannelCipher, ChannelCipher)>,
    health: Arc<SessionHealth>,
) -> Option<JoinHandle<()>> {
    if event_port == 0 {
        debug!("AirPlay 2: no eventPort advertised; skipping event channel");
        return None;
    }
    let addr = SocketAddr::new(receiver_ip, event_port);
    let stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        Ok(s) => s,
        Err(e) => {
            warn!("AirPlay 2: event channel connect to {} failed: {}", addr, e);
            return None;
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    debug!("AirPlay 2: event channel open to {}", addr);

    std::thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-event:{}", receiver_name))
        .spawn(move || {
            use std::io::{Read, Write};
            let mut stream = stream;
            let mut buf = [0u8; 2048];
            let mut chunks: u64 = 0;
            let mut decoder = ciphers.map(|(reader, writer)| EventChannel::new(reader, writer));
            while !stop_flag.load(Ordering::Acquire) {
                match stream.read(&mut buf) {
                    Ok(0) => {
                        // Receiver closed the channel: evidence of when this
                        // member went away, should the row die.
                        let at = Instant::now();
                        info!(
                            "AP2 {}: event channel closed by the receiver, {}",
                            receiver_name,
                            health.since_record_at(at)
                        );
                        health.note_failure(&receiver_name, "closed its event channel", at);
                        break;
                    }
                    Ok(n) => {
                        chunks += 1;
                        if let Some(dec) = decoder.as_mut() {
                            match dec.feed(&buf[..n]) {
                                Ok(requests) => {
                                    for (req, reply) in requests {
                                        if reply.is_empty() {
                                            info!("AP2 event {}: {} (a response; not answered)", receiver_name, req);
                                            continue;
                                        }
                                        let sent = stream.write_all(&reply).and_then(|_| stream.flush());
                                        info!(
                                            "AP2 event {}: {} -> {}",
                                            receiver_name,
                                            req,
                                            match &sent {
                                                Ok(()) => "replied 200".to_string(),
                                                Err(e) => format!("reply failed: {}", e),
                                            }
                                        );
                                    }
                                    continue;
                                }
                                Err(e) => {
                                    warn!(
                                        "AP2 event {}: cannot decrypt the event channel ({:#}); logging \
                                         raw arrivals only from here",
                                        receiver_name, e
                                    );
                                    decoder = None;
                                }
                            }
                        }
                        // Diagnostic: receivers can send requests on this
                        // channel; if one gates playback, discarding it
                        // silently would look exactly like our silence bug.
                        let preview_len = n.min(64);
                        let hex: String =
                            buf[..preview_len].iter().map(|b| format!("{:02x}", b)).collect();
                        let text: String = buf[..preview_len]
                            .iter()
                            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                            .collect();
                        if chunks <= 3 {
                            info!(
                                "AirPlay 2 event channel: received {} bytes (hex {} | text {:?})",
                                n, hex, text
                            );
                        } else {
                            debug!("AirPlay 2 event channel: {} bytes", n);
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        let at = Instant::now();
                        info!(
                            "AP2 {}: event channel error ({}), {}",
                            receiver_name,
                            e,
                            health.since_record_at(at)
                        );
                        health.note_failure(&receiver_name, &format!("event channel error ({})", e), at);
                        break;
                    }
                }
            }
            debug!("AirPlay 2 event channel closed ({} chunks received)", chunks);
        })
        .ok()
}

/// The decrypting side of the event channel: the receiver's framed HAP
/// blocks in, complete RTSP requests out, each with the encrypted
/// `200 OK` to send back.
struct EventChannel {
    reader: ChannelCipher,
    writer: ChannelCipher,
    /// Encrypted bytes not yet forming a whole block.
    enc: Vec<u8>,
    /// Decrypted bytes not yet forming a whole request.
    plain: Vec<u8>,
}

/// One request the receiver sent on the event channel (for the log).
#[derive(Debug, Clone, PartialEq, Eq)]
struct EventRequest {
    method: String,
    uri: String,
    cseq: Option<String>,
    /// The body: a plist rendered on one line, text, or its size.
    body: String,
}

impl std::fmt::Display for EventRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} {}", self.method, self.uri, self.body)
    }
}

impl EventChannel {
    fn new(reader: ChannelCipher, writer: ChannelCipher) -> Self {
        Self { reader, writer, enc: Vec::new(), plain: Vec::new() }
    }

    /// Feed bytes read from the socket. Returns every request completed
    /// by them, each with its encrypted reply. A decryption failure is
    /// final (the nonce counters can't be resynchronised).
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<(EventRequest, Vec<u8>)>> {
        self.enc.extend_from_slice(bytes);
        while self.enc.len() >= 2 {
            let len = u16::from_le_bytes([self.enc[0], self.enc[1]]);
            let need = 2 + len as usize + TAG_LEN;
            if self.enc.len() < need {
                break;
            }
            let plain = self.reader.decrypt_block(len, &self.enc[2..need])?;
            self.plain.extend_from_slice(&plain);
            self.enc.drain(..need);
        }
        let mut out = Vec::new();
        while let Some((req, used)) = parse_event_request(&self.plain)? {
            self.plain.drain(..used);
            if req.method.starts_with("RTSP/") {
                // A response (status line first), not a request: nothing
                // to answer.
                out.push((req, Vec::new()));
                continue;
            }
            let mut reply = String::from("RTSP/1.0 200 OK\r\n");
            if let Some(cseq) = &req.cseq {
                reply.push_str(&format!("CSeq: {}\r\n", cseq));
            }
            reply.push_str("Server: StreamToSpeaker/1.0\r\nContent-Length: 0\r\n\r\n");
            let wire = self.writer.encrypt(reply.as_bytes());
            out.push((req, wire));
        }
        Ok(out)
    }
}

/// Parse one complete RTSP request off the front of `buf`: `Some((request,
/// bytes used))`, or `None` when more bytes are needed.
fn parse_event_request(buf: &[u8]) -> Result<Option<(EventRequest, usize)>> {
    let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        if buf.len() > 1 << 16 {
            anyhow::bail!("event channel: {} bytes without a complete request header", buf.len());
        }
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("?").to_string();
    let uri = parts.next().unwrap_or("?").to_string();
    let mut cseq = None;
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            if k.eq_ignore_ascii_case("CSeq") {
                cseq = Some(v.trim().to_string());
            } else if k.eq_ignore_ascii_case("Content-Length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let body_start = head_end + 4;
    if buf.len() < body_start + content_length {
        return Ok(None);
    }
    let body = &buf[body_start..body_start + content_length];
    let body = if body.is_empty() {
        "(no body)".to_string()
    } else if body.starts_with(b"bplist") {
        match plist::from_bytes::<plist::Value>(body) {
            Ok(v) => plist_brief(&v),
            Err(_) => format!("<{} byte plist, unparseable>", body.len()),
        }
    } else if body.iter().all(|&b| b == b'\r' || b == b'\n' || (0x20..0x7f).contains(&b)) {
        format!("{:?}", String::from_utf8_lossy(body))
    } else {
        format!("<{} bytes>", body.len())
    };
    Ok(Some((EventRequest { method, uri, cseq, body }, body_start + content_length)))
}

/// Whether a failed `/feedback` ends the member, and with what reason
/// prefix: after three failures in a row, or — with `end_on_close`
/// (pair/group sessions) — at once when the receiver closed or reset the
/// control connection. `consecutive` counts this failure.
fn feedback_ends_member(end_on_close: bool, connection_closed: bool, consecutive: u32) -> Option<&'static str> {
    if end_on_close && connection_closed {
        Some("feedback")
    } else if consecutive == 3 {
        Some("feedback x3")
    } else {
        None
    }
}

/// POST /feedback every ~2 s until the session stops. Failures downgrade
/// to debug after the first warn — a dropped keepalive shouldn't spam.
/// Three failures in a row end the member; with `end_on_close` (pair/group
/// sessions) a closed or reset control connection ends it at once. Either
/// way the member's end is dated by the first failure of the run, not the
/// one that ended it.
fn spawn_feedback_keepalive(
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    stop_flag: Arc<AtomicBool>,
    health: Arc<SessionHealth>,
    receiver_name: String,
    end_on_close: bool,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-feedback:{}", receiver_name))
        .spawn(move || {
            let mut warned = false;
            let mut consecutive_failures = 0u32;
            let mut first_failure_at: Option<Instant> = None;
            let mut ended = false;
            'outer: loop {
                // Sleep in slices so shutdown isn't delayed.
                let slices = (FEEDBACK_INTERVAL.as_millis() / 100) as u32;
                for _ in 0..slices {
                    if stop_flag.load(Ordering::Acquire) {
                        break 'outer;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let result = rtsp.lock().unwrap().feedback();
                if let Err(e) = result {
                    let at = *first_failure_at.get_or_insert_with(Instant::now);
                    if !warned {
                        warn!("AirPlay 2 /feedback to {} failed (continuing): {:#}", receiver_name, e);
                        warned = true;
                    } else {
                        debug!("AirPlay 2 /feedback to {} failed: {:#}", receiver_name, e);
                    }
                    consecutive_failures += 1;
                    // The control channel is gone: failing three times
                    // running, or (pair/group) closed/reset now. Flag the
                    // session dead for the app watchdog, saying which
                    // member and why.
                    if !ended {
                        if let Some(what) =
                            feedback_ends_member(end_on_close, rtsp_connection_closed(&e), consecutive_failures)
                        {
                            ended = true;
                            health.member_ended(&receiver_name, &format!("{}: {}", what, rtsp_failure_reason(&e)), at);
                        }
                    }
                } else {
                    consecutive_failures = 0;
                    first_failure_at = None;
                }
            }
            debug!("AirPlay 2 feedback keepalive exiting");
        })
        .ok()
}

// ---------------------------------------------------------------------------
// Buffered audio sender (type 103 — length-prefixed sealed packets on TCP)
// ---------------------------------------------------------------------------

/// Payload codec for the buffered stream. AAC-LC is what iOS sends (and
/// the only codec field-proven on Sonos); ALAC needs no encoder and is
/// the fallback + the non-Windows dev default.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BufferedCodecKind {
    Alac,
    Aac,
}

impl BufferedCodecKind {
    fn label(self) -> &'static str {
        match self {
            BufferedCodecKind::Alac => "ALAC",
            BufferedCodecKind::Aac => "AAC-LC",
        }
    }

    /// Samples per packet: fixed at 1024 by AAC-LC; 352 for ALAC (RAOP
    /// convention, matches the SETUP `spf`).
    fn spf(self) -> usize {
        match self {
            BufferedCodecKind::Alac => FRAMES_PER_PACKET,
            BufferedCodecKind::Aac => 1024,
        }
    }
}

/// Turns one spf-sized chunk of interleaved PCM into payload frames.
enum PayloadEncoder {
    /// Uncompressed ALAC: always exactly one frame per chunk.
    Alac,
    #[cfg(windows)]
    Aac(crate::airplay::aac_mf::AacEncoder),
}

impl PayloadEncoder {
    fn new(codec: BufferedCodecKind) -> Result<Self> {
        match codec {
            BufferedCodecKind::Alac => Ok(PayloadEncoder::Alac),
            #[cfg(windows)]
            BufferedCodecKind::Aac => Ok(PayloadEncoder::Aac(crate::airplay::aac_mf::AacEncoder::new()?)),
            #[cfg(not(windows))]
            BufferedCodecKind::Aac => anyhow::bail!("AAC is only negotiated on Windows"),
        }
    }

    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>> {
        match self {
            PayloadEncoder::Alac => Ok(vec![build_uncompressed_alac_frame(pcm)]),
            #[cfg(windows)]
            PayloadEncoder::Aac(enc) => enc.encode(pcm),
        }
    }
}

struct BufferedSenderConfig {
    /// Every member's TCP data connection + key, first half first.
    outlets: Vec<Outlet>,
    /// The distinct channel maps (see [`Outlet::payload`]); one encoder
    /// each. `[Stereo]` unless the L/R split is on.
    maps: Vec<ChannelMap>,
    initial_seq: u16,
    initial_rtptime: u32,
    ssrc: u32,
    samples_rx: Receiver<PcmFrame>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
    current_rtptime: Arc<AtomicU32>,
    /// Last RTP sequence number sent — read at stop() for FLUSHBUFFERED.
    last_seq: Arc<AtomicU32>,
    /// Shared RTSP connections — the sender anchors on them at first
    /// audio, one identical anchor each.
    rtsps: Vec<Arc<Mutex<Ap2Rtsp>>>,
    /// Both PTP timelines (ours + the receiver's, once locked).
    timeline: PtpTimeline,
    /// Negotiated payload codec (must match the SETUP's ct/spf).
    codec: BufferedCodecKind,
    health: Arc<SessionHealth>,
    /// PTP follow mode: anchor only on a followed member's clock (never on
    /// ours, which follow mode does not serve), and re-anchor when the
    /// followed clock is relocked.
    follow: bool,
}

/// Send SETRATEANCHORTIME for the buffered stream ("`a.rtp` plays at
/// `a.network_ns` on `a.timeline_id`") to every member. Every member gets
/// the SAME anchor (one rtpTime, one instant, one clock id) — that is what
/// makes a stereo pair start in lock-step. Every member is tried and its
/// answer logged; the first refusal is returned with the member's
/// name, and the caller retries the anchor on every member (re-anchoring
/// an accepting member is harmless — the values are simply newer).
fn send_anchor(
    rtsps: &[Arc<Mutex<Ap2Rtsp>>],
    names: &[String],
    a: &Anchor,
) -> std::result::Result<(), (String, anyhow::Error)> {
    let mut first_err: Option<(String, anyhow::Error)> = None;
    for (rtsp, name) in rtsps.iter().zip(names) {
        let result = rtsp.lock().unwrap().set_rate_anchor_time(1, a.rtp, a.network_ns, a.timeline_id);
        match result {
            Ok(()) => info!("AP2 anchor -> {}: 200 OK (timeline {:#018x})", name, a.timeline_id),
            Err(e) => {
                info!("AP2 anchor -> {}: {:#}", name, e);
                first_err.get_or_insert((name.clone(), e));
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A playback anchor shared by every member of a session: `rtp` plays at
/// `network_ns` on the timeline `timeline_id`, which is the instant
/// `our_ns` on our own clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    timeline_id: u64,
    network_ns: u64,
    rtp: u32,
    our_ns: u64,
    /// Epoch of the followed clock the anchor is on (None = our clock).
    epoch: Option<u64>,
}

/// The anchor for "now": `rtp` plays `ANCHOR_LEAD_NS` ahead, rounded up to
/// a whole second so networkTimeFrac is 0. On the followed receiver's
/// clock (`(id, offset, epoch)`, see [`PtpTimeline::followed_epoch`]) when
/// the PTP layer follows one — field-tested: Sonos refuses anchors on any
/// other clock — else on ours (receivers that follow the sender). In
/// `master` mode with two or more members the PTP layer never follows a
/// member, so the anchor is on our grandmaster clock; in `follow` mode it
/// is only ever on the followed member's.
fn compute_anchor(followed: Option<(u64, i64, u64)>, our_clock_id: u64, our_now_ns: u64, rtp: u32) -> Anchor {
    let (timeline_id, offset, epoch) = match followed {
        Some((id, off, e)) => (id, off, Some(e)),
        None => (our_clock_id, 0, None),
    };
    let base_ns = (our_now_ns as i128 + offset as i128).max(0) as u64;
    let network_ns = (base_ns + ANCHOR_LEAD_NS).div_ceil(1_000_000_000) * 1_000_000_000;
    let our_ns = (network_ns as i128 - offset as i128).max(0) as u64;
    Anchor { timeline_id, network_ns, rtp, our_ns, epoch }
}

/// Follow mode: `a` re-expressed on the followed clock once that clock was
/// (re)locked after `a` was made — another grandmaster, or a step — so the
/// receivers keep playing on a clock that exists. The stream-to-wall
/// mapping stays the same: the new anchor is a future whole second of the
/// new timeline, `ANCHOR_LEAD_NS` ahead, with the rtptime that plays then
/// on `a`'s mapping (±½ sample). None while `a`'s lock still holds, or
/// while no clock is followed.
fn reanchor(a: &Anchor, followed: Option<(u64, i64, u64)>, our_now_ns: u64) -> Option<Anchor> {
    let (id, off, epoch) = followed?;
    if a.epoch == Some(epoch) {
        return None;
    }
    let next = compute_anchor(Some((id, off, epoch)), 0, our_now_ns, 0);
    let elapsed_ns = next.our_ns as i128 - a.our_ns as i128;
    let samples = (elapsed_ns * WIRE_SAMPLE_RATE as i128 + 500_000_000).div_euclid(1_000_000_000);
    Some(Anchor { rtp: a.rtp.wrapping_add(samples as i64 as u32), ..next })
}

/// Frame one sealed RTP packet for the buffered TCP stream: a 2-byte
/// big-endian length prefix that **includes itself** (the reference
/// receiver reads 2 bytes, then `len - 2` more).
fn frame_buffered_packet(pkt: &[u8]) -> Vec<u8> {
    let total = (pkt.len() + 2) as u16;
    let mut out = Vec::with_capacity(pkt.len() + 2);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(pkt);
    out
}

fn spawn_ap2_buffered_sender(cfg: BufferedSenderConfig) -> Result<JoinHandle<()>> {
    let name = format!("stream-to-speaker-ap2-buffered:{}", cfg.receiver_name);
    Ok(std::thread::Builder::new().name(name).spawn(move || run_ap2_buffered_sender(cfg))?)
}

fn run_ap2_buffered_sender(mut cfg: BufferedSenderConfig) {
    use std::io::Write;
    info!(
        "AirPlay 2 buffered sender → {} (seq={}, rtptime={})",
        cfg.outlets
            .iter()
            .map(|o| match &o.dest {
                OutletDest::Tcp(s) => format!("{:?}", s.peer_addr().ok()),
                OutletDest::Udp { addr, .. } => addr.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" + "),
        cfg.initial_seq,
        cfg.initial_rtptime
    );
    let spf = cfg.codec.spf();
    let mut seq = cfg.initial_seq;
    let mut rtptime = cfg.initial_rtptime;
    let mut packet_count: u64 = 0;
    let mut ring: Vec<i16> = Vec::with_capacity(spf * 2);

    // One encoder per channel map (AAC is stateful, so each mix needs its
    // own); with the split off that is exactly one. The AAC encoder must
    // live on this thread (COM); the session already probed availability
    // before negotiating ct=4 in SETUP.
    let mut encoders: Vec<PayloadEncoder> = Vec::with_capacity(cfg.maps.len());
    for _ in &cfg.maps {
        match PayloadEncoder::new(cfg.codec) {
            Ok(enc) => encoders.push(enc),
            Err(e) => {
                warn!("AirPlay 2: {} encoder init failed on sender thread: {e:#}", cfg.codec.label());
                return;
            }
        }
    }
    // Encoded frames per map, waiting until every map has one: packet
    // N carries frame N of every mix, so a mix whose encoder primes a
    // frame later can't shift one member's timeline against another's.
    let mut queues: Vec<std::collections::VecDeque<Vec<u8>>> =
        cfg.maps.iter().map(|_| std::collections::VecDeque::new()).collect();
    let names: Vec<String> = cfg.outlets.iter().map(|o| o.name.clone()).collect();

    let started = Instant::now();
    let packet_duration =
        Duration::from_nanos((spf as u64 * 1_000_000_000) / WIRE_SAMPLE_RATE as u64);
    let mut idle_warned = false;
    let mut anchor: Option<Anchor> = None;
    let mut follow_wait_logged = false;
    // Pacing baseline — reset at the anchor so packet deadlines line up
    // with the promised playback timeline.
    let mut pace_start = Instant::now();
    let mut last_anchor_try: Option<Instant> = None;

    loop {
        if cfg.stop_flag.load(Ordering::Acquire) {
            break;
        }
        match cfg.samples_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(frame) => {
                append_samples(&mut ring, &frame);
                loop {
                    match cfg.samples_rx.try_recv() {
                        Ok(f) => append_samples(&mut ring, &f),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if anchor.is_none() && !idle_warned && started.elapsed() > Duration::from_secs(3) {
                    warn!(
                        "AirPlay 2: no audio from the source after 3s — is something playing with \
                         Stream To Speaker selected as the Windows output device?"
                    );
                    idle_warned = true;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }

        // Anchor at first audio: "this rtptime plays at (timeline now +
        // lead)" is only true if the packet carrying that rtptime is about
        // to be sent. Until anchored, don't send — the receiver has no
        // timeline for the bytes.
        if anchor.is_none() {
            if ring.len() >= spf * 2
                && last_anchor_try.map_or(true, |t| t.elapsed() >= Duration::from_secs(1))
            {
                last_anchor_try = Some(Instant::now());
                let followed = cfg.timeline.followed_epoch();
                if cfg.follow && followed.is_none() {
                    if !follow_wait_logged {
                        follow_wait_logged = true;
                        info!(
                            "AirPlay 2 PTP follow: no member's clock is followed yet — the buffered anchor \
                             waits for one (never our own clock, which follow mode does not serve); \
                             retrying every 1 s"
                        );
                    }
                } else {
                    let a = compute_anchor(followed, cfg.timeline.clock_id, cfg.timeline.our_now_ns(), rtptime);
                    match send_anchor(&cfg.rtsps, &names, &a) {
                        Ok(()) => {
                            anchor = Some(a);
                            pace_start = Instant::now();
                            packet_count = 0;
                            info!(
                                "AirPlay 2: anchored at first audio — rtpTime {} on timeline {:#018x}{}",
                                rtptime,
                                a.timeline_id,
                                if a.epoch.is_none() { " (our clock)" } else { " (receiver's clock)" }
                            );
                        }
                        Err((name, e)) => {
                            warn!("AirPlay 2 anchor failed on {} (will retry): {:#}", name, e);
                        }
                    }
                }
            }
            if anchor.is_none() {
                // Bound the pre-anchor buffer to ~2 s of the freshest audio.
                let max = WIRE_SAMPLE_RATE as usize * 2 * 2;
                if ring.len() > max {
                    let cut = ring.len() - max;
                    ring.drain(..cut);
                }
                continue;
            }
        } else if cfg.follow {
            // The followed clock was relocked (another grandmaster, a step,
            // another member after the followed one went quiet): the anchor
            // names a timeline that no longer holds. Re-state the same
            // mapping on the new one, or end the row if a member refuses.
            let current = anchor.expect("anchored");
            if let Some(next) = reanchor(&current, cfg.timeline.followed_epoch(), cfg.timeline.our_now_ns()) {
                match send_anchor(&cfg.rtsps, &names, &next) {
                    Ok(()) => {
                        info!(
                            "AirPlay 2 PTP follow: the followed clock changed ({:#018x} → {:#018x}) — \
                             re-anchored rtpTime {} on it, same playback instant",
                            current.timeline_id, next.timeline_id, next.rtp
                        );
                        anchor = Some(next);
                    }
                    Err((name, e)) => {
                        cfg.health.member_ended(
                            &name,
                            &format!("re-anchor on the followed clock refused ({:#})", e),
                            Instant::now(),
                        );
                        return;
                    }
                }
            }
        }

        // Silence-fill: the anchored timeline equates rtptime with wall
        // time, so a starved source (nothing playing on Windows) must not
        // stall rtptime — synthesize silence to keep the receiver's buffer
        // primed and the mapping intact.
        if ring.len() < spf * 2 {
            let deadline = pace_start + packet_duration.saturating_mul((packet_count + 1) as u32);
            if Instant::now() >= deadline {
                ring.resize(ring.len() + spf * 2, 0);
            }
        }

        let mut packets_this_round = 0u32;
        while ring.len() >= spf * 2 {
            let pkt_samples: Vec<i16> = ring.drain(..spf * 2).collect();
            // One spf-sized PCM chunk → zero or more payload frames per
            // map (the AAC MFT buffers a frame or two before its first
            // output; ALAC is always 1:1).
            for ((map, enc), queue) in cfg.maps.iter().zip(encoders.iter_mut()).zip(queues.iter_mut()) {
                match enc.encode(&map.apply_cow(&pkt_samples)) {
                    Ok(frames) => queue.extend(frames),
                    Err(e) => {
                        warn!("AirPlay 2: {} encode failed: {e:#}", cfg.codec.label());
                        return;
                    }
                }
            }

            while queues.iter().all(|q| !q.is_empty()) {
                let payloads: Vec<Vec<u8>> = queues.iter_mut().map(|q| q.pop_front().unwrap()).collect();
                let header = ap2_rtp_header(seq, rtptime, cfg.ssrc, packet_count == 0);

                // Pace to wall-clock from the anchor: the receiver plays
                // rtptime-at-anchor 0.5-1.5 s from now, so staying at the
                // sample rate keeps its buffer bounded on both sides.
                let deadline = pace_start + packet_duration.saturating_mul((packet_count + 1) as u32);
                let now = Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline - now);
                }

                // Same header (and payload, per channel map) to every
                // member, sealed per key.
                let packets = seal_for_outlets(&cfg.outlets, &header, seq, &payloads);
                for (outlet, packet) in cfg.outlets.iter_mut().zip(&packets) {
                    let framed = frame_buffered_packet(packet);
                    let OutletDest::Tcp(stream) = &mut outlet.dest else {
                        unreachable!("buffered outlets are TCP");
                    };
                    if let Err(e) = stream.write_all(&framed) {
                        warn!(
                            "AirPlay 2 buffered send to {} failed (receiver stopped reading?): {}",
                            outlet.name, e
                        );
                        cfg.health.member_ended(&outlet.name, &format!("data write error ({})", e), Instant::now());
                        return;
                    }
                    if packet_count == 0 {
                        info!(
                            "AirPlay 2: buffered {} audio flowing to {} — first packet ({} bytes framed)",
                            cfg.codec.label(),
                            outlet.name,
                            framed.len()
                        );
                    }
                }

                seq = seq.wrapping_add(1);
                rtptime = rtptime.wrapping_add(spf as u32);
                cfg.current_rtptime.store(rtptime, Ordering::Release);
                cfg.last_seq.store(seq as u32, Ordering::Release);
                packet_count += 1;
                if packet_count % 500 == 0 {
                    debug!(
                        "AirPlay 2: {} buffered packets sent ({} s)",
                        packet_count,
                        packet_count * spf as u64 / WIRE_SAMPLE_RATE as u64
                    );
                }
            }
            packets_this_round += 1;
            if packets_this_round > 32 {
                break;
            }
        }
    }
    info!("AirPlay 2 buffered sender stopped after {} packets", packet_count);
}

// ---------------------------------------------------------------------------
// Audio sender (ChaCha20-Poly1305 realtime ALAC)
// ---------------------------------------------------------------------------

struct Ap2SenderConfig {
    /// Every member's UDP destination + key, first half first.
    outlets: Vec<Outlet>,
    /// The distinct channel maps (see [`Outlet::payload`]). `[Stereo]`
    /// unless the L/R split is on.
    maps: Vec<ChannelMap>,
    initial_seq: u16,
    initial_rtptime: u32,
    ssrc: u32,
    samples_rx: Receiver<PcmFrame>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
    current_rtptime: Arc<AtomicU32>,
    health: Arc<SessionHealth>,
    /// Pair/group sessions: the send schedule the session's sync sender
    /// stamps its packets from. The sender paces to its start and
    /// publishes the next packet after every send. None for a single
    /// receiver (paced from the thread's own start, as always).
    schedule: Option<Arc<SendSchedule>>,
}

fn spawn_ap2_sender(cfg: Ap2SenderConfig) -> Result<JoinHandle<()>> {
    let name = format!("stream-to-speaker-ap2-rtp:{}", cfg.receiver_name);
    Ok(std::thread::Builder::new().name(name).spawn(move || run_ap2_sender(cfg))?)
}

fn run_ap2_sender(cfg: Ap2SenderConfig) {
    info!(
        "AirPlay 2 RTP sender → {} (seq={}, rtptime={})",
        cfg.outlets
            .iter()
            .map(|o| match &o.dest {
                OutletDest::Udp { addr, .. } => addr.to_string(),
                OutletDest::Tcp(s) => format!("{:?}", s.peer_addr().ok()),
            })
            .collect::<Vec<_>>()
            .join(" + "),
        cfg.initial_seq,
        cfg.initial_rtptime
    );
    let mut seq = cfg.initial_seq;
    let mut rtptime = cfg.initial_rtptime;
    let mut packet_count: u64 = 0;
    let mut silence_packets: u64 = 0;
    let mut got_real_audio = false;
    let mut idle_warned = false;
    let mut ring: Vec<i16> = Vec::with_capacity(FRAMES_PER_PACKET * 4);
    let mut disconnected = false;
    let stereo = FRAMES_PER_PACKET * 2;

    // Drop frames queued during the multi-second pairing/SETUP handshake —
    // paced sending never drains a backlog, so it would be permanent latency.
    while cfg.samples_rx.try_recv().is_ok() {}

    let start = cfg.schedule.as_ref().map_or_else(Instant::now, |s| s.start());
    let packet_duration = realtime_packet_duration();

    // Deadline-driven with silence-fill — identical discipline to the RAOP
    // sender (rtp.rs `run_sender`): the rtptime must stay glued to
    // wall-clock or the 1 Hz sync re-stamps a frozen timestamp against
    // advancing time and the receiver discards everything as late (the
    // anchor-burn failure proven on this device family).
    loop {
        if cfg.stop_flag.load(Ordering::Acquire) {
            break;
        }
        let deadline = start + packet_duration.saturating_mul((packet_count + 1) as u32);

        // Re-glue after a long stall (suspend/resume) instead of flooding.
        let behind = Instant::now().saturating_duration_since(deadline);
        if behind > Duration::from_secs(1) {
            let missed = (behind.as_nanos() / packet_duration.as_nanos().max(1)) as u64 + 1;
            packet_count += missed;
            rtptime = rtptime.wrapping_add((missed as u32).wrapping_mul(FRAMES_PER_PACKET as u32));
            cfg.current_rtptime.store(rtptime, Ordering::Release);
            if let Some(s) = &cfg.schedule {
                s.set_next(rtptime, packet_count);
            }
            ring.clear();
            while cfg.samples_rx.try_recv().is_ok() {}
            warn!(
                "AirPlay 2 RTP sender: stalled {:.1}s; skipped {} packet slots to stay glued \
                 to wall-clock",
                behind.as_secs_f32(),
                missed
            );
            continue;
        }

        loop {
            loop {
                match cfg.samples_rx.try_recv() {
                    Ok(f) => append_samples(&mut ring, &f),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected || ring.len() >= stereo {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = (deadline - now).min(Duration::from_millis(50));
            match cfg.samples_rx.recv_timeout(wait) {
                Ok(f) => append_samples(&mut ring, &f),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if cfg.stop_flag.load(Ordering::Acquire) {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => disconnected = true,
            }
        }
        if cfg.stop_flag.load(Ordering::Acquire) {
            break;
        }
        if disconnected && ring.len() < stereo {
            break;
        }

        // Diagnose an upstream that never produces audio (nothing playing /
        // wrong default device) — the AirPlay stream itself is fine.
        if !got_real_audio && !idle_warned && start.elapsed() > Duration::from_secs(3) {
            warn!(
                "AirPlay 2: no audio from the source after 3s — is something playing with \
                 Stream To Speaker selected as the Windows output device? (streaming silence \
                 to keep the timeline anchored)"
            );
            idle_warned = true;
        }

        // Cap accumulated backlog (mirrors rtp.rs run_sender): each
        // transient underrun inserts a silence packet ahead of the late
        // real samples, so without a cap per-underrun latency creeps
        // unboundedly over a long session. Drop the oldest samples once
        // the ring exceeds ~32 ms.
        let high_water = stereo * 4;
        let drop_to = stereo * 2;
        if ring.len() > high_water {
            let drop = ring.len() - drop_to;
            ring.drain(..drop);
            debug!("AirPlay 2 RTP sender: dropped {} samples of backlog to cap latency", drop);
        }

        let pkt_samples: Vec<i16> = if ring.len() >= stereo {
            got_real_audio = true;
            ring.drain(..stereo).collect()
        } else {
            silence_packets += 1;
            vec![0i16; stereo]
        };
        // One ALAC frame per channel map (just one unless the L/R split
        // is on); the same header to every member, sealed per key.
        let payloads: Vec<Vec<u8>> =
            cfg.maps.iter().map(|m| build_uncompressed_alac_frame(&m.apply_cow(&pkt_samples))).collect();
        let header = ap2_rtp_header(seq, rtptime, cfg.ssrc, packet_count == 0);
        let packets = seal_for_outlets(&cfg.outlets, &header, seq, &payloads);

        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }

        for (outlet, packet) in cfg.outlets.iter().zip(&packets) {
            let OutletDest::Udp { socket, addr } = &outlet.dest else {
                unreachable!("realtime outlets are UDP");
            };
            if let Err(e) = socket.send_to(packet, addr) {
                warn!("AirPlay 2 RTP send to {} failed: {}", outlet.name, e);
                cfg.health.member_ended(&outlet.name, &format!("RTP send error ({})", e), Instant::now());
                return;
            }
            if let Some(resend) = &outlet.resend {
                resend.record(seq, packet);
            }
            if packet_count == 0 {
                info!(
                    "AirPlay 2: stream open — first packet ({} bytes) sent to {}",
                    packet.len(),
                    addr
                );
            }
        }

        seq = seq.wrapping_add(1);
        rtptime = rtptime.wrapping_add(FRAMES_PER_PACKET as u32);
        cfg.current_rtptime.store(rtptime, Ordering::Release);
        packet_count += 1;
        if let Some(s) = &cfg.schedule {
            s.set_next(rtptime, packet_count);
        }
    }
    info!(
        "AirPlay 2 RTP sender stopped after {} packets ({} silence-filled)",
        packet_count, silence_packets
    );
}

/// Build the 12-byte RTP header for an AirPlay 2 realtime audio packet
/// (V=2, PT=96, marker on the first packet only).
fn ap2_rtp_header(seq: u16, timestamp: u32, ssrc: u32, first: bool) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80; // V=2
    h[1] = if first { 0x80 | 0x60 } else { 0x60 }; // marker? + PT=96
    BigEndian::write_u16(&mut h[2..4], seq);
    BigEndian::write_u32(&mut h[4..8], timestamp);
    BigEndian::write_u32(&mut h[8..12], ssrc);
    h
}

fn append_samples(ring: &mut Vec<i16>, frame: &PcmFrame) {
    let bytes = &**frame.0;
    let n = bytes.len() / 2;
    let start = ring.len();
    ring.resize(start + n, 0);
    for i in 0..n {
        ring[start + i] = i16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::pair_experiments::PairRecipe;

    #[test]
    fn buffered_framing_length_includes_itself() {
        let pkt = [0xAAu8; 10];
        let framed = frame_buffered_packet(&pkt);
        assert_eq!(framed.len(), 12);
        // Receiver reads 2-byte BE length, then len-2 more bytes.
        assert_eq!(u16::from_be_bytes([framed[0], framed[1]]), 12);
        assert_eq!(&framed[2..], &pkt);
    }

    fn member(name: &str, mac: &str, gpn: Option<&str>) -> AirPlayRenderer {
        let mut group = crate::airplay::discovery::GroupHints::default();
        group.group_name = gpn.map(str::to_string);
        AirPlayRenderer {
            friendly_name: name.into(),
            mac_id: mac.into(),
            ip: "192.0.2.1".parse().unwrap(),
            port: 0,
            airplay_port: Some(7000),
            encryption_types: vec![],
            codecs: vec![],
            password_protected: false,
            encryption_key_required: false,
            features: None,
            pk: None,
            model: Some("AudioAccessory5,1".into()),
            group,
        }
    }

    #[test]
    fn anchor_is_one_instant_on_one_clock_for_every_member() {
        // Our clock: lead of 0.5 s, rounded UP to a whole second.
        let a = compute_anchor(None, 0x1234, 1_700_000_000, 77);
        assert_eq!(a.timeline_id, 0x1234);
        assert_eq!(a.network_ns, 3_000_000_000);
        assert_eq!((a.our_ns, a.rtp, a.epoch), (3_000_000_000, 77, None));
        // Exactly on a second boundary after the lead: no extra second.
        let a = compute_anchor(None, 0x1234, 2_500_000_000, 0);
        assert_eq!(a.network_ns, 3_000_000_000);
        // Receiver clock preferred when the PTP layer has locked onto one
        // (single-receiver Sonos behaviour); its id names the timeline.
        let a = compute_anchor(Some((0xABCD, 10_100_000_000 - 7, 1)), 0x1234, 7, 5);
        assert_eq!(
            a,
            Anchor { timeline_id: 0xABCD, network_ns: 11_000_000_000, rtp: 5, our_ns: 900_000_007, epoch: Some(1) }
        );
        // The anchor is a value, computed once — every member of a pair
        // gets a byte-identical SETRATEANCHORTIME by construction.
        assert_eq!(compute_anchor(None, 9, 123, 1), compute_anchor(None, 9, 123, 1));
    }

    #[test]
    fn follow_mode_reanchors_on_a_relocked_clock_at_the_same_playback_instant() {
        // Anchored on member A's clock (offset +1000 s, epoch 1): rtp 1000
        // plays at our 2.0 s.
        let a = compute_anchor(Some((0xA, 1_000_000_000_000, 1)), 0x42, 1_300_000_000, 1000);
        assert_eq!(a.our_ns, 2_000_000_000);
        // Same lock (drift only): nothing to do. No clock followed: wait.
        assert_eq!(reanchor(&a, Some((0xA, 1_000_000_000_050, 1)), 5_000_000_000), None);
        assert_eq!(reanchor(&a, None, 5_000_000_000), None);
        // Relocked on B (offset +7 s, epoch 2) at our 5.3 s: the new anchor
        // is on B, a future whole second of B's timeline …
        let b = reanchor(&a, Some((0xB, 7_000_000_000, 2)), 5_300_000_000).unwrap();
        assert_eq!((b.timeline_id, b.epoch), (0xB, Some(2)));
        assert_eq!(b.network_ns % 1_000_000_000, 0);
        assert_eq!(b.network_ns, 13_000_000_000);
        assert_eq!(b.our_ns, 6_000_000_000);
        // … with the rtptime that plays then on the ORIGINAL mapping:
        // 4.0 s after rtp 1000 → 1000 + 4 × 44100.
        assert_eq!(b.rtp, 1000 + 4 * 44100);
        // Wraps like every rtptime.
        let w = compute_anchor(None, 1, 0, u32::MAX - 10);
        let w2 = reanchor(&Anchor { epoch: Some(1), ..w }, Some((2, 0, 2)), 1_000_000_000).unwrap();
        assert_eq!(w2.rtp, 44100 - 11);
    }

    #[test]
    fn setpeers_names_that_receiver_then_us() {
        let receiver: IpAddr = "192.0.2.11".parse().unwrap();
        let other_half: IpAddr = "192.0.2.12".parse().unwrap();
        let us: IpAddr = "192.0.2.2".parse().unwrap();
        // Single receivers and `ma`: the receiver, then us — the other
        // half is NOT in this receiver's list (OwnTone / airplay-cli
        // per-session shape).
        for recipe in [None, Some(PairRecipe::Ma)] {
            let (list, ct) = setpeers(recipe, receiver, &[other_half], us);
            assert_eq!(list, vec![receiver, us]);
            assert_eq!(ct, "application/x-apple-binary-plist");
        }
        let (list, ct) = setpeers(Some(PairRecipe::Apple), receiver, &[other_half], us);
        assert_eq!(list, vec![receiver, other_half, us]);
        assert_eq!(ct, "/peer-list-changed");
    }

    #[test]
    fn seal_fan_out_shares_header_and_payload_but_not_the_seal() {
        let socket = || UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let outlet = |key: u8, resend: bool, payload: usize| Outlet {
            name: format!("half {key}"),
            audio_key: [key; 32],
            dest: OutletDest::Udp { socket: socket(), addr },
            resend: resend.then(|| ResendBuffer::new(4)),
            payload,
        };
        let outlets = vec![outlet(1, true, 0), outlet(2, true, 0), outlet(1, false, 0)];
        let header = ap2_rtp_header(7, 44100, 0xDEADBEEF, true);
        let payload = build_uncompressed_alac_frame(&vec![0x0102i16; FRAMES_PER_PACKET * 2]);
        let payloads = vec![payload.clone()];
        let packets = seal_for_outlets(&outlets, &header, 7, &payloads);
        assert_eq!(packets.len(), 3);
        for p in &packets {
            assert_eq!(&p[..12], &header, "same RTP seq/timestamp on every member");
            assert_eq!(p.len(), 12 + payload.len() + 16 + 8, "ciphertext + tag + nonce");
        }
        assert_ne!(packets[0][12..], packets[1][12..], "different keys → different seals");
        assert_eq!(packets[0], packets[2], "same key → identical bytes (deterministic)");
        // A single outlet is exactly today's one packet.
        let single = seal_for_outlets(&outlets[..1], &header, 7, &payloads);
        assert_eq!(single, vec![packets[0].clone()]);
    }

    #[test]
    fn channel_split_shares_the_timeline_but_not_the_payload() {
        let socket = || UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        // Same key on both outlets so only the payload can differ.
        let outlet = |payload: usize| Outlet {
            name: format!("half {payload}"),
            audio_key: [5; 32],
            dest: OutletDest::Udp { socket: socket(), addr },
            resend: None,
            payload,
        };
        let (maps, index) = distinct_maps(&[ChannelMap::Left, ChannelMap::Right]);
        assert_eq!(maps, vec![ChannelMap::Left, ChannelMap::Right]);
        assert_eq!(index, vec![0, 1]);
        let pcm: Vec<i16> = (0..FRAMES_PER_PACKET * 2).map(|i| if i % 2 == 0 { 100 } else { -100 }).collect();
        let payloads: Vec<Vec<u8>> =
            maps.iter().map(|m| build_uncompressed_alac_frame(&m.apply_cow(&pcm))).collect();
        assert_ne!(payloads[0], payloads[1]);
        let outlets = vec![outlet(index[0]), outlet(index[1])];
        let header = ap2_rtp_header(9, 1234, 1, false);
        let packets = seal_for_outlets(&outlets, &header, 9, &payloads);
        assert_eq!(&packets[0][..12], &packets[1][..12], "one seq/RTP timeline");
        assert_ne!(packets[0][12..], packets[1][12..], "L and R payloads");
        // Without a split every member maps to the one stereo payload.
        assert_eq!(distinct_maps(&[ChannelMap::Stereo; 3]), (vec![ChannelMap::Stereo], vec![0, 0, 0]));
        assert_eq!(distinct_maps(&[]), (vec![ChannelMap::Stereo], vec![]));
        assert_eq!(
            distinct_maps(&[ChannelMap::Right, ChannelMap::Stereo, ChannelMap::Right]),
            (vec![ChannelMap::Right, ChannelMap::Stereo], vec![0, 1, 0])
        );
    }

    #[test]
    fn event_channel_decrypts_requests_and_answers_200() {
        use crate::airplay::ap2_crypto::SessionKeys;
        let keys = SessionKeys::from_shared(&[0x42u8; 64]);
        let (mut rx_writer, mut rx_reader) = keys.receiver_event_ciphers();
        let mut ch = EventChannel::new(keys.event_reader(), keys.event_writer());

        // A plist-bodied request, delivered one byte at a time.
        let mut body_dict = plist::Dictionary::new();
        body_dict.insert("type".into(), "sendMediaRemoteCommand".into());
        body_dict.insert("value".into(), "paus".into());
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &plist::Value::Dictionary(body_dict)).unwrap();
        let mut req = format!(
            "POST /command RTSP/1.0\r\nCSeq: 7\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        req.extend_from_slice(&body);
        let wire = rx_writer.encrypt(&req);
        let mut got = Vec::new();
        for b in &wire {
            got.extend(ch.feed(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(got.len(), 1);
        let (r, reply) = &got[0];
        assert_eq!(r.method, "POST");
        assert_eq!(r.uri, "/command");
        assert_eq!(r.cseq.as_deref(), Some("7"));
        assert_eq!(r.body, r#"{type: "sendMediaRemoteCommand", value: "paus"}"#);
        assert_eq!(r.to_string(), r#"POST /command {type: "sendMediaRemoteCommand", value: "paus"}"#);
        // The receiver can decrypt our reply: a 200 echoing its CSeq.
        let len = u16::from_le_bytes([reply[0], reply[1]]);
        let plain = rx_reader.decrypt_block(len, &reply[2..]).unwrap();
        let text = String::from_utf8(plain).unwrap();
        assert!(text.starts_with("RTSP/1.0 200 OK\r\nCSeq: 7\r\n"), "{text}");
        assert!(text.ends_with("Content-Length: 0\r\n\r\n"), "{text}");

        // Two body-less requests in one read → two replies, in order.
        let wire = rx_writer.encrypt(b"GET /a RTSP/1.0\r\nCSeq: 8\r\n\r\nGET /b RTSP/1.0\r\nCSeq: 9\r\n\r\n");
        let got = ch.feed(&wire).unwrap();
        assert_eq!(got.iter().map(|(r, _)| r.uri.as_str()).collect::<Vec<_>>(), vec!["/a", "/b"]);
        assert_eq!(got[0].0.body, "(no body)");

        // A response on the channel is logged, never answered.
        let wire = rx_writer.encrypt(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n");
        let got = ch.feed(&wire).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.method, "RTSP/1.0");
        assert!(got[0].1.is_empty());

        // Garbage (wrong key) fails decryption instead of looping.
        let other = SessionKeys::from_shared(&[0x43u8; 64]);
        let (mut wrong_writer, _) = other.receiver_event_ciphers();
        assert!(ch.feed(&wrong_writer.encrypt(b"GET / RTSP/1.0\r\n\r\n")).is_err());
    }

    #[test]
    fn member_failure_is_logged_once_and_kills_the_row() {
        let h = SessionHealth::default();
        assert!(!h.is_dead());
        assert_eq!(h.since_record_at(Instant::now()), "before RECORD");
        h.note_record();
        let when = h.since_record_at(Instant::now());
        assert!(when.ends_with("s after RECORD"), "{when}");
        let now = Instant::now();
        h.member_ended("Links", "RTP send error (x)", now);
        h.member_ended("Rechts", "feedback x3: RTSP EOF (y)", now + Duration::from_secs(1));
        assert!(h.is_dead());
        let first = h.first_failure().unwrap();
        assert!(first.starts_with("Links RTP send error (x) ("), "{first}");
    }

    #[test]
    fn first_failure_is_the_earliest_seen_not_the_first_declared() {
        // Rechts drops 2.0 s after RECORD, Links 2.4 s. Links' keepalive
        // happens to declare its end first (thread order); the row's first
        // failure still names Rechts, with the time Rechts' failure was
        // first seen.
        let h = SessionHealth::default();
        let record = Instant::now();
        *h.record_at.lock().unwrap() = Some(record);
        let rechts_at = record + Duration::from_millis(2000);
        let links_at = record + Duration::from_millis(2400);
        assert_eq!(h.first_failure(), None, "nothing to report while the row is alive");
        h.note_failure("Rechts", "closed its event channel", rechts_at);
        assert_eq!(h.first_failure(), None);
        h.member_ended("Links", "feedback: RTSP EOF (eof)", links_at);
        h.member_ended("Rechts", "feedback: RTSP EOF (eof)", rechts_at);
        assert_eq!(h.first_failure().unwrap(), "Rechts closed its event channel (2.0 s after RECORD)");
        assert_eq!(h.since_record_at(links_at), "2.4 s after RECORD");
        assert_eq!(h.since_record_at(record - Duration::from_millis(500)), "0.5 s before RECORD");
    }

    #[test]
    fn old_failure_evidence_is_not_the_first_failure() {
        // An event channel the receiver closed a minute before the row
        // died ended nothing; the member end is the first failure. Evidence
        // within the window before it still counts.
        let h = SessionHealth::default();
        let record = Instant::now();
        *h.record_at.lock().unwrap() = Some(record);
        h.note_failure("Kitchen", "closed its event channel", record + Duration::from_secs(1));
        h.member_ended("Kitchen", "feedback x3: timeout", record + Duration::from_secs(61));
        assert_eq!(h.first_failure().unwrap(), "Kitchen feedback x3: timeout (61.0 s after RECORD)");
        h.note_failure("Rechts", "closed its event channel", record + Duration::from_secs(55));
        assert_eq!(h.first_failure().unwrap(), "Rechts closed its event channel (55.0 s after RECORD)");
    }

    #[test]
    fn feedback_ends_a_single_receiver_after_three_failures_and_a_pair_member_on_the_first_eof() {
        // Single receiver: three failures in a row, closed connection or
        // not.
        assert_eq!(feedback_ends_member(false, true, 1), None);
        assert_eq!(feedback_ends_member(false, true, 2), None);
        assert_eq!(feedback_ends_member(false, true, 3), Some("feedback x3"));
        assert_eq!(feedback_ends_member(false, false, 3), Some("feedback x3"));
        // Pair/group: a closed or reset connection ends the member at
        // once; any other failure still waits for three.
        assert_eq!(feedback_ends_member(true, true, 1), Some("feedback"));
        assert_eq!(feedback_ends_member(true, false, 1), None);
        assert_eq!(feedback_ends_member(true, false, 3), Some("feedback x3"));
    }

    #[test]
    fn rtsp_failures_name_a_closed_connection() {
        let eof = anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "failed to fill whole buffer"));
        assert!(rtsp_failure_reason(&eof).starts_with("RTSP EOF"));
        let reset = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset)).context("/feedback");
        assert!(rtsp_failure_reason(&reset).starts_with("RTSP EOF"));
        let other = anyhow::anyhow!("/feedback → 500 Internal Server Error");
        assert_eq!(rtsp_failure_reason(&other), "/feedback → 500 Internal Server Error");
        // In a pair/group session a closed connection ends the member at
        // once; a timeout or an error status waits for three in a row.
        assert!(rtsp_connection_closed(&eof));
        assert!(rtsp_connection_closed(&reset));
        assert!(!rtsp_connection_closed(&other));
        let timeout = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::TimedOut));
        assert!(!rtsp_connection_closed(&timeout));
    }

    #[test]
    fn session_names_a_pair_by_group_name_else_halves() {
        let l = member("Links", "AA", Some("Büro 2"));
        let r = member("Rechts", "BB", Some("Büro 2"));
        assert_eq!(session_display_name(&[l.clone()]), "Links");
        assert_eq!(session_display_name(&[l.clone(), r.clone()]), "Büro 2");
        assert_eq!(session_label(&[l.clone(), r.clone()]), "Links + Rechts");
        let l2 = member("Links", "AA", Some("  "));
        assert_eq!(session_display_name(&[l2, r.clone()]), "Links + Rechts");
        let l3 = member("Links", "AA", None);
        assert_eq!(session_display_name(&[l3, r]), "Links + Rechts");
    }

    #[test]
    fn ap2_header_layout() {
        let h = ap2_rtp_header(0x1234, 0xAABBCCDD, 0x01020304, true);
        assert_eq!(h[0], 0x80);
        assert_eq!(h[1], 0xE0); // marker + 0x60
        assert_eq!(&h[2..4], &[0x12, 0x34]);
        assert_eq!(&h[4..8], &[0xAA, 0xBB, 0xCC, 0xDD]);
        let h2 = ap2_rtp_header(1, 2, 3, false);
        assert_eq!(h2[1], 0x60); // no marker
    }
}
