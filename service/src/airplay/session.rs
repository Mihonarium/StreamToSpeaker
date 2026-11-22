//! High-level AirPlay session lifecycle.
//!
//! Mirrors `app::start_session` / `app::stop_session` for the UPnP
//! path. One `AirPlaySession` owns one open RTSP connection, three UDP
//! sockets (audio, control, timing), and the background audio sender
//! thread. Dropping the session tears it all down.

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use log::{debug, info, warn};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::airplay::crypto::{Cipher, MfiKey};
use crate::airplay::discovery::AirPlayRenderer;
use crate::airplay::rtp::{
    bind_udp, random_initial_rtptime, random_initial_seq, random_ssrc, spawn_audio_sender,
    RtpSenderConfig,
};
use crate::airplay::rtsp::{AbandonHandle, ReceiverBusy, RtspClient, RtspError, ServerPorts, SignIn, TEARDOWN_LIMIT};
use crate::airplay::timing::{
    sleep_unless_stopped, spawn_resend_responder, spawn_sync_sender, spawn_timing_responder,
    ResendBuffer,
};
use crate::http_server::PcmFrame;

/// Recently-sent packets retained for retransmit (~4 s at 44.1 kHz).
const RESEND_BUFFER_PACKETS: usize = 512;

/// Default playback latency in samples for the sync-packet anchor. 88200
/// = 2 s at 44.1 kHz — packet-capture verified: iTunes→Sonos sync packets
/// carry exactly `next_rtptime − 88200` (and node_airtunes2 hardcodes
/// `2 * sampling_rate`). The user's `airplay_latency_ms` overrides it.
#[cfg(test)]
const DEFAULT_LATENCY_SAMPLES: u32 = 88200;

/// The sync-anchor latency (samples) to use given the receiver's
/// advertised `Audio-Latency` (`None` if it never sent one) and the
/// configured target. Mirrors libraop's on-wire sync value:
/// `max(reported, configured)` — a receiver asking for a deeper buffer
/// (big-DSP AVRs) gets it, and a low-latency target can never drive a
/// receiver below its own stated floor; everyone else (Sonos and most
/// speakers omit the header) gets exactly the target. NB the `+11025` in
/// libraop is a sender-side *scheduling* value (`raopcl_latency`) that
/// never reaches the sync packet, so we don't add it here.
fn effective_latency_samples(advertised: Option<u32>, target: u32) -> u32 {
    advertised.unwrap_or(0).max(target)
}

/// A receiver may state `Audio-Latency` at SETUP, at RECORD, or both
/// (Apple TV / AirPort Express do; a big-DSP AVR only at RECORD). Keep the
/// larger — the floor we must not anchor below.
fn merge_advertised_latency(setup: Option<u32>, record: Option<u32>) -> Option<u32> {
    match (setup, record) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Configuration to spin up one AirPlay session.
pub struct AirPlaySessionConfig {
    pub renderer: AirPlayRenderer,
    /// The receiver's addresses to try, best first (see
    /// [`crate::airplay::discovery::order_addresses`]), each with the
    /// local address this PC uses toward it: the one UDP sockets are bound
    /// to and SDP advertises (this PC's address on the receiver's
    /// network, or `--advertise-ip`). The next is tried when the TCP
    /// connection can't be made; at most [`MAX_ROUTES`].
    pub routes: Vec<Route>,
    /// Subscription on the StreamHub — produces PcmFrame entries the
    /// audio thread consumes.
    pub samples_rx: Receiver<PcmFrame>,
    /// Initial volume in [0, 100] — converted to RAOP dB internally.
    pub initial_volume: Option<u32>,
    /// Time limits for the connect and the session's requests; the app
    /// uses [`RaopTimeouts::STANDARD`].
    pub timeouts: RaopTimeouts,
    /// The opener that worked for this receiver last time, if it wasn't
    /// the usual one (see [`next_opener_memory`]).
    pub opener_hint: Option<Opener>,
    /// Opt-in `et=4` MFi-encryption experiment (config flag). When true
    /// and the receiver advertises et=4, the session tries the MFi
    /// wrapped-key handshake first, falling back to plaintext/RSA on
    /// failure. Off by default: the wrap is a best-grounded guess (no
    /// open-source reference implements et=4), and a stalled attempt
    /// wedges this receiver class for tens of seconds.
    pub mfi_encryption: bool,
    /// Debug escape hatch: send uncompressed-ALAC escape frames instead
    /// of real compressed ALAC. See `rtp.rs` module docs.
    pub uncompressed_alac: bool,
    /// AirPlay password for a `pw=true` receiver (RTSP Digest auth).
    /// `None` for the common unprotected case.
    pub password: Option<String>,
    /// Sync-anchor latency target in ms (user_config `airplay_latency_ms`,
    /// already clamped). 2000 = the iTunes-proven anchor; lower cuts the
    /// receiver's buffer, and thus the audible delay, in step. The
    /// receiver's advertised `Audio-Latency` is honoured as a floor.
    pub latency_ms: u32,
}

/// One way to reach a receiver: its address, and this PC's address
/// toward it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub receiver: IpAddr,
    pub local: IpAddr,
}

/// Most of a receiver's addresses one connect tries.
pub const MAX_ROUTES: usize = 3;

/// The routes of a connect: `first` (the address discovery listed the
/// receiver at, or one a person added), then the receiver's other
/// announced addresses that `allowed` passes (only what can be a device's,
/// see [`crate::airplay::discovery::could_be_a_device`]), each with this
/// PC's address toward it; [`MAX_ROUTES`] in all.
pub fn routes_for(
    renderer: &AirPlayRenderer,
    first: Route,
    allowed: impl Fn(IpAddr) -> bool,
    local_toward: impl Fn(IpAddr) -> Option<IpAddr>,
) -> Vec<Route> {
    let mut routes = vec![first];
    for &a in &renderer.raop.addresses {
        if routes.len() >= MAX_ROUTES {
            break;
        }
        if a == first.receiver || !allowed(a) {
            continue;
        }
        if let Some(local) = local_toward(a) {
            routes.push(Route { receiver: a, local });
        }
    }
    routes
}

/// Time limits of one AirPlay 1 connect and session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RaopTimeouts {
    /// The TCP connect and the first request (the opener). Short: a
    /// receiver whose AirPlay 1 service is vestigial (a Sonos, where the
    /// app then moves on to AirPlay 2) never answers the opener, while a
    /// working one answers within a LAN round trip.
    pub opener: Duration,
    /// Each request after the opener: ANNOUNCE, SETUP, RECORD, the
    /// keepalive, volume. A receiver that answered the opener is there,
    /// and some take seconds to answer SETUP or RECORD while they switch
    /// input or wake an amplifier. OwnTone waits 15 s
    /// (`RTSP_READ_TIMEOUT`).
    pub reply: Duration,
    /// TEARDOWN (see [`TEARDOWN_LIMIT`]).
    pub teardown: Duration,
    /// Between an attempt that failed and the next one on another
    /// connection, so the receiver has let go of the first.
    pub retry_pause: Duration,
    /// Before the one retry a busy receiver (453) gets, when the session
    /// it is busy with is likely this app's own (see
    /// `left_session_recently`). OwnTone reconnects a lost speaker after
    /// 5 s (`PLAYER_SPEAKER_RESURRECT_TIME`).
    pub busy_retry: Duration,
}

impl RaopTimeouts {
    pub const STANDARD: Self = Self {
        opener: Duration::from_secs(3),
        reply: Duration::from_secs(15),
        teardown: TEARDOWN_LIMIT,
        retry_pause: Duration::from_secs(2),
        busy_retry: Duration::from_secs(5),
    };
}

/// The receiver refused the connect with 453 and stayed busy. Its
/// message is the one the person reads.
#[derive(Debug)]
pub struct Busy {
    pub name: String,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{name} is busy. Another device may be playing to it, or it hasn't let go of this PC's last \
             connection yet. Try again in a minute; if it stays busy, restart {name}.",
            name = self.name
        )
    }
}

impl std::error::Error for Busy {}

/// The receiver wants a sign-in the connect couldn't give. Its message
/// is the one the person reads.
#[derive(Debug)]
pub struct SignInNeeded {
    pub name: String,
    pub why: SignIn,
}

impl std::fmt::Display for SignInNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = &self.name;
        match self.why {
            SignIn::PasswordNeeded => write!(
                f,
                "{name} asks for its AirPlay password. Pick {name} in the Stream To Speaker window to enter it."
            ),
            SignIn::PasswordRejected => write!(
                f,
                "{name} didn't accept the AirPlay password. Pick {name} in the Stream To Speaker window to enter it again."
            ),
            SignIn::Unsupported { .. } => write!(
                f,
                "{name} asks for a kind of sign-in this app can't do. To use it here, turn off its password or access \
                 setting in the speaker's own app."
            ),
        }
    }
}

impl std::error::Error for SignInNeeded {}

/// How long after this app left a session on a receiver a 453 from it is
/// taken to be about that session.
const OWN_SESSION_WINDOW: Duration = Duration::from_secs(60);

/// When this app last left a session on each receiver (by stable id): a
/// session that stopped, or an attempt whose ANNOUNCE was accepted and
/// that then failed. A receiver that answers 453 soon after may still be
/// holding it.
fn left_sessions() -> &'static Mutex<std::collections::HashMap<String, Instant>> {
    static LEFT: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Instant>>> = std::sync::OnceLock::new();
    LEFT.get_or_init(Default::default)
}

fn note_left_session(id: &str) {
    let mut left = left_sessions().lock().unwrap_or_else(|e| e.into_inner());
    left.retain(|_, at| at.elapsed() < OWN_SESSION_WINDOW);
    left.insert(id.to_string(), Instant::now());
}

/// Did this app leave a session on receiver `id` within the last minute?
fn left_session_recently(id: &str) -> bool {
    left_sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .is_some_and(|at| at.elapsed() < OWN_SESSION_WINDOW)
}

/// How an attempt opens the RTSP conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Opener {
    /// `OPTIONS *` with an Apple-Challenge: receivers without MFi or
    /// AirPlay 2, as iTunes opens them.
    Options,
    /// `POST /auth-setup`, which must succeed: MFi (`et` lists 4) and
    /// AirPlay 2 receivers, as iTunes opens a Sonos (packet capture).
    AuthSetup,
    /// The order of the reference senders (OwnTone; libraop skips the
    /// OPTIONS): `OPTIONS *`, then `POST /auth-setup` only when the
    /// reply's `Public` lists POST, and a failed `/auth-setup` passed over.
    OptionsThenAuthSetup,
}

/// The openers to try, in order. The first is the one that has always
/// been used for this kind of receiver, unless `hint` says the other one
/// worked last time. An MFi receiver without AirPlay 2 (third-party
/// docks and AV receivers) gets a second attempt the reference senders'
/// way when the first fails; Sonos-class receivers don't (their fallback
/// is AirPlay 2), nor do plain ones.
fn opener_plan(renderer: &AirPlayRenderer, hint: Option<Opener>) -> Vec<Opener> {
    let mfi = renderer.encryption_types.contains(&4);
    let usual = if mfi || renderer.supports_airplay2() { Opener::AuthSetup } else { Opener::Options };
    if !mfi || renderer.supports_airplay2() {
        return vec![usual];
    }
    match hint {
        Some(Opener::OptionsThenAuthSetup) => vec![Opener::OptionsThenAuthSetup, usual],
        _ => vec![usual, Opener::OptionsThenAuthSetup],
    }
}

/// What to remember about a receiver's opener after a connect: the
/// reference order when it is the one that worked; nothing after a
/// connect that worked the usual way, or one that failed (a remembered
/// order that fails is forgotten).
pub fn next_opener_memory(used: Option<Opener>) -> Option<Opener> {
    used.filter(|o| *o == Opener::OptionsThenAuthSetup)
}

/// Read a remembered opener leniently: anything unknown is "nothing
/// remembered", never an unreadable settings file.
pub fn deserialize_opener_lenient<'de, D>(d: D) -> Result<Option<Opener>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = <serde_json::Value as serde::Deserialize>::deserialize(d)?;
    Ok(serde_json::from_value::<Option<Opener>>(v).unwrap_or(None))
}

/// How long `stop()` waits for a request in flight to finish before it
/// cuts it short. A keepalive or volume change takes milliseconds; one
/// still waiting after this is stalled.
const STOP_GRACE: Duration = Duration::from_secs(1);

/// A connect that has run this long tells the person what it is waiting
/// for (see [`AirPlaySession::start_with_notices`]).
const SLOW_CONNECT: Duration = Duration::from_secs(10);

/// A wait for one reply is reported only once it has lasted this long,
/// so a request answered at once never flashes a notice.
const SLOW_REPLY: Duration = Duration::from_secs(2);

/// What a connect is doing, for the notices of a slow one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Between requests.
    Working,
    /// Waiting at most `limit` for the receiver (a TCP connect or a
    /// reply).
    Waiting { limit: Duration },
    /// Pausing `limit` before another attempt.
    Pausing { why: Pause, limit: Duration },
}

/// Why a connect pauses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pause {
    /// An attempt failed; the next one opens another way.
    AnotherWay,
    /// The receiver is busy; one retry follows.
    Busy,
}

/// The stage of a connect in progress, shared with the thread that
/// reports a slow one.
struct Progress {
    state: Mutex<(Stage, Instant, bool)>,
    changed: Condvar,
}

impl Progress {
    fn new() -> Self {
        Self { state: Mutex::new((Stage::Working, Instant::now(), false)), changed: Condvar::new() }
    }

    fn set(&self, stage: Stage) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.0 = stage;
        st.1 = Instant::now();
        self.changed.notify_all();
    }

    /// Run `f` while waiting at most `limit` for the receiver.
    fn waiting<T>(&self, limit: Duration, f: impl FnOnce() -> T) -> T {
        self.set(Stage::Waiting { limit });
        let out = f();
        self.set(Stage::Working);
        out
    }

    /// Pause `limit` before another attempt.
    fn pause(&self, why: Pause, limit: Duration) {
        self.set(Stage::Pausing { why, limit });
        std::thread::sleep(limit);
        self.set(Stage::Working);
    }

    fn finish(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).2 = true;
        self.changed.notify_all();
    }

    /// Until [`finish`](Self::finish): once the connect has run
    /// [`SLOW_CONNECT`], pass `notice` a line for each wait that lasts
    /// [`SLOW_REPLY`].
    fn report(&self, started: Instant, notice: &(dyn Fn(&str) + Sync)) {
        self.report_after(started, SLOW_CONNECT, SLOW_REPLY, notice)
    }

    fn report_after(&self, started: Instant, slow_connect: Duration, slow_reply: Duration, notice: &(dyn Fn(&str) + Sync)) {
        let mut reported: Option<Instant> = None;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while !st.2 {
            let now = Instant::now();
            let (stage, since) = (st.0, st.1);
            let waited = now.saturating_duration_since(since);
            let left = |limit: Duration| limit.saturating_sub(waited).as_secs_f32().ceil() as u64;
            let line = match stage {
                Stage::Waiting { limit } if waited >= slow_reply => Some(match left(limit) {
                    0 | 1 => "it hasn't answered yet; waiting a moment longer.".to_string(),
                    n => format!("it hasn't answered yet; waiting up to {} more seconds.", n),
                }),
                Stage::Pausing { why: Pause::AnotherWay, .. } => {
                    Some("the first try didn't work; trying once more another way.".to_string())
                }
                Stage::Pausing { why: Pause::Busy, limit } => Some(match left(limit) {
                    0 | 1 => "it is busy; trying once more.".to_string(),
                    n => format!("it is busy; trying once more in {} seconds.", n),
                }),
                _ => None,
            };
            if let Some(line) = line.filter(|_| now >= started + slow_connect && reported != Some(since)) {
                reported = Some(since);
                drop(st);
                notice(&line);
                st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                continue;
            }
            st = self.changed.wait_timeout(st, Duration::from_millis(250)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// Lock `m`, waiting at most `within` for another thread to let go.
fn lock_within<T>(m: &Mutex<T>, within: Duration) -> Option<MutexGuard<'_, T>> {
    let end = Instant::now() + within;
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() >= end => return None,
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Live AirPlay session.
pub struct AirPlaySession {
    pub renderer: AirPlayRenderer,
    /// Held under a Mutex so volume commands from arbitrary threads
    /// can serialise into the single RTSP connection.
    rtsp: Arc<Mutex<RtspClient>>,
    /// Signals every background thread to stop (audio sender, timing
    /// responder, sync sender, resend responder, keepalive).
    stop_flag: Arc<AtomicBool>,
    /// Set by the background threads when the session has demonstrably
    /// died (audio socket send error, or repeated keepalive failures on
    /// the RTSP connection). The app-level watchdog polls this to tear
    /// down the zombie and auto-reconnect.
    dead: Arc<AtomicBool>,
    /// Retransmission counters, fed by the resend responder.
    resend_stats: Arc<crate::airplay::timing::ResendStats>,
    /// Current RTP write head — read to stamp the `RTP-Info` on metadata
    /// SET_PARAMETERs.
    current_rtptime: Arc<AtomicU32>,
    /// Last volume (0..=100) pushed to the receiver; what an unmute
    /// restores. RAOP has no separate mute, only the -144 dB sentinel.
    volume_pct: Arc<AtomicU32>,
    /// The background threads. All of them watch `stop_flag` with
    /// bounded wakeups, so `stop()` just sets the flag and joins.
    threads: Vec<JoinHandle<()>>,
    /// Cuts a stalled request on the control connection short, so
    /// `stop()` never waits for its full time limit.
    abandon: AbandonHandle,
    /// The opener this session's connect used.
    opener: Opener,
    /// The sync anchor's latency (samples): how far the write head runs
    /// ahead of what plays.
    latency_samples: u32,
    timeouts: RaopTimeouts,
    /// Hold the audio socket so it isn't closed prematurely; the
    /// sender thread holds a `try_clone` for sending. Once `stop`
    /// runs we drop these in the right order to wake any blocked
    /// background thread.
    _audio_socket: UdpSocket,
}

/// Stops-and-joins the session's background threads if `start()` errors
/// out partway through spawning them — without this, a failed spawn (or
/// any later `?`) would leak an already-running audio sender that keeps
/// streaming to the receiver with no owner. `into_threads()` defuses the
/// guard on the success path.
struct SpawnGuard {
    stop_flag: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl SpawnGuard {
    fn new(stop_flag: Arc<AtomicBool>) -> Self {
        Self {
            stop_flag,
            threads: Vec::new(),
        }
    }

    fn adopt(&mut self, handle: JoinHandle<()>) {
        self.threads.push(handle);
    }

    fn into_threads(mut self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut self.threads)
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if self.threads.is_empty() {
            return;
        }
        self.stop_flag.store(true, Ordering::Release);
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}

impl AirPlaySession {
    /// Open the full RAOP session: RTSP handshake, allocate UDP
    /// sockets, RECORD, and spawn the audio thread. Returns once audio
    /// is being delivered.
    pub fn start(cfg: AirPlaySessionConfig) -> Result<Self> {
        Self::start_with_notices(cfg, &|_| {})
    }

    /// [`start`](Self::start), passing `notice` a line (to follow `Still
    /// connecting to <name> — `) whenever a connect that has already run
    /// `SLOW_CONNECT` (10 s) is waiting on the receiver, so the window can say
    /// what the wait is for.
    pub fn start_with_notices(cfg: AirPlaySessionConfig, notice: &(dyn Fn(&str) + Sync)) -> Result<Self> {
        let progress = Progress::new();
        let started = Instant::now();
        std::thread::scope(|scope| {
            // Without a thread for them (none left), the connect just
            // goes without notices.
            let _ = std::thread::Builder::new()
                .name("stream-to-speaker-airplay-connect-notices".into())
                .spawn_scoped(scope, || progress.report(started, notice));
            let out = Self::start_inner(cfg, &progress);
            progress.finish();
            out
        })
    }

    fn start_inner(mut cfg: AirPlaySessionConfig, progress: &Progress) -> Result<Self> {
        info!(
            "AirPlay: starting session to {} ({}:{})",
            cfg.renderer.friendly_name, cfg.renderer.ip, cfg.renderer.port,
        );
        // Once per connect, so a report of a receiver that won't connect
        // carries what it announces.
        if cfg.renderer.raop.txt.is_empty() {
            info!("AirPlay: {} announces no AirPlay 1 TXT record (added by address, or not announced)", cfg.renderer.friendly_name);
        } else {
            info!("AirPlay: {} announces _raop._tcp TXT: {}", cfg.renderer.friendly_name, cfg.renderer.raop.txt_line());
        }

        if !cfg.renderer.supports_legacy_raop() {
            return Err(anyhow::anyhow!(
                "speaker {} doesn't advertise a codec/encryption pair we support \
                 (codecs={:?}, et={:?}, password={})",
                cfg.renderer.friendly_name,
                cfg.renderer.codecs,
                cfg.renderer.encryption_types,
                cfg.renderer.password_protected,
            ));
        }

        // The first connection decides which of the receiver's addresses
        // the session uses, and with it this PC's local address.
        let (first, route) = connect_first(&cfg, progress)?;
        cfg.renderer.ip = route.receiver;

        // Bind the three UDP sockets before the handshake so we know our
        // ports for SETUP.
        let audio_socket = bind_udp(route.local).context("bind audio UDP")?;
        let control_socket = bind_udp(route.local).context("bind control UDP")?;
        let timing_socket = bind_udp(route.local).context("bind timing UDP")?;
        let audio_port = audio_socket.local_addr()?.port();
        let control_port = control_socket.local_addr()?.port();
        let timing_port = timing_socket.local_addr()?.port();
        debug!(
            "AirPlay UDP sockets bound: audio={}, control={}, timing={}",
            audio_port, control_port, timing_port
        );

        // Random initial RTP state for this session — generated once and
        // reused across handshake attempts and the audio sender.
        let initial_seq = random_initial_seq();
        let initial_rtptime = random_initial_rtptime();
        let ssrc = random_ssrc();

        // The timing responder must be live BEFORE the handshake: this
        // receiver class probes our SETUP-advertised timing port with NTP
        // requests while SETUP is still in flight (packet-capture: the
        // Sonos fires three 0xD2s 24 ms after the SETUP request, before
        // its SETUP 200; iTunes answers each within 200 µs). libraop
        // carries the same lesson as a comment: "AppleTV expects now the
        // timing port to be opened BEFORE the setup message". Our old
        // responder-after-RECORD ordering left those probes unanswered in
        // every stalled-SETUP session on record.
        let stop_flag = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let resend_stats = Arc::new(crate::airplay::timing::ResendStats::default());
        let mut guard = SpawnGuard::new(stop_flag.clone());
        guard.adopt(
            spawn_timing_responder(
                timing_socket,
                stop_flag.clone(),
                cfg.renderer.friendly_name.clone(),
            )
            .context("spawning AirPlay timing responder")?,
        );

        let want_mfi = cfg.renderer.encryption_types.contains(&4);
        // The et=4 attempt is opt-in: the wrap is a best-grounded guess,
        // and a stalled attempt wedges this receiver class for tens of
        // seconds (capture-proven: after one SETUP stall the Sonos
        // answers zero RTSP bytes on ANY connection for the hold window),
        // so it must never gate default connectivity.
        let try_mfi_first = cfg.mfi_encryption && want_mfi;

        let timeouts = cfg.timeouts;
        let bringup = Bringup {
            cfg: &cfg,
            route,
            first: std::cell::RefCell::new(Some(first)),
            progress,
            control_port,
            timing_port,
            initial_seq,
            initial_rtptime,
            target_latency: crate::airplay::timing::latency_ms_to_samples(cfg.latency_ms),
        };
        // On any error below, `guard`'s Drop stops and joins whatever
        // threads exist — the timing responder now, the sync/audio/resend
        // threads once adopted.
        let mut failed: Vec<String> = Vec::new();
        let mut handshake = None;
        let id = cfg.renderer.stable_id();
        let name = &cfg.renderer.friendly_name;
        if try_mfi_first {
            match bringup.attempt(How::Mfi) {
                Ok(h) => handshake = Some(h),
                Err(f) => {
                    warn!(
                        "AirPlay: et=4 MFi handshake to {} failed ({:#}); \
                         retrying with plaintext/RSA",
                        name, f.error
                    );
                    failed.push(format!("MFi encryption: {:#}", f.error));
                    if f.announced {
                        // It got past ANNOUNCE and was torn down: give the
                        // receiver a moment to let go of it.
                        progress.pause(Pause::AnotherWay, timeouts.retry_pause);
                    }
                }
            }
        }
        if handshake.is_none() {
            let plan = opener_plan(&cfg.renderer, cfg.opener_hint);
            let mut next = 0;
            let mut busy_retried = false;
            while let Some(&opener) = plan.get(next) {
                match bringup.attempt(How::Plain(opener)) {
                    Ok(h) => {
                        handshake = Some(h);
                        break;
                    }
                    Err(f) if f.busy => {
                        // Busy isn't about how the connect opens: another
                        // opener won't help. One delayed retry, only when the
                        // session it is busy with is likely this app's own.
                        warn!("AirPlay: {} is busy ({:#})", name, f.error);
                        if busy_retried || !left_session_recently(&id) {
                            return Err(anyhow::Error::new(Busy { name: name.clone() }));
                        }
                        busy_retried = true;
                        info!("AirPlay: {} may still hold this PC's last session; trying once more", name);
                        progress.pause(Pause::Busy, timeouts.busy_retry);
                    }
                    Err(f) if f.error.downcast_ref::<SignIn>().is_some() => {
                        // A password problem isn't about how the connect
                        // opens either.
                        let why = f.error.downcast_ref::<SignIn>().cloned().unwrap_or(SignIn::PasswordNeeded);
                        warn!("AirPlay: {} wants a sign-in ({:#})", name, f.error);
                        return Err(anyhow::Error::new(SignInNeeded { name: name.clone(), why }));
                    }
                    Err(f) => {
                        // Another opener can only help a receiver that took
                        // the connection and then didn't get as far as
                        // answering RECORD.
                        let another_way = f.connected && !f.record_answered;
                        if plan.len() > 1 {
                            failed.push(format!("opening with {}: {:#}", opener.describe(), f.error));
                        } else {
                            failed.push(format!("{:#}", f.error));
                        }
                        next += 1;
                        if !another_way || next >= plan.len() {
                            break;
                        }
                        if let Some(o) = plan.get(next) {
                            info!("AirPlay: trying {} again, opening with {}", name, o.describe());
                        }
                        progress.pause(Pause::AnotherWay, timeouts.retry_pause);
                    }
                }
            }
        }
        let Some(Handshake { mut rtsp, cipher, ports: server_ports, latency: latency_samples, opener }) = handshake else {
            return Err(anyhow::anyhow!("{}", failed.join("; then ")));
        };
        info!(
            "AirPlay: cipher={} (receiver et={:?}, mfi_experiment={})",
            cipher.label(),
            cfg.renderer.encryption_types,
            cfg.mfi_encryption,
        );
        let cipher = Arc::new(cipher);
        debug!(
            "AirPlay SETUP returned server ports: audio={}, control={}, timing={}",
            server_ports.audio, server_ports.control, server_ports.timing,
        );

        if let Some(vol) = cfg.initial_volume {
            // Best-effort — don't fail the session if the volume set
            // bounces (some receivers are picky about timing). It waits
            // only as long as the opener: the session is up, and a reply
            // that comes later is skipped by its CSeq.
            let db = volume_pct_to_raop_db(vol);
            rtsp.set_reply_limit(timeouts.opener);
            if let Err(e) = rtsp.set_volume(db) {
                warn!("AirPlay SET_PARAMETER volume failed: {:#}", e);
            }
            rtsp.set_reply_limit(timeouts.reply);
        }
        let abandon = rtsp.abandon_handle().context("keeping a handle on the RTSP connection")?;

        let receiver_control_addr = SocketAddr::new(cfg.renderer.ip, server_ports.control);

        // Spin up the background threads. The audio sender owns a
        // try_clone of the audio socket; sync owns the control socket
        // outright.
        let audio_socket_for_thread = audio_socket
            .try_clone()
            .context("try_clone audio socket")?;
        let current_rtptime = Arc::new(AtomicU32::new(initial_rtptime));
        let resend = ResendBuffer::new(RESEND_BUFFER_PACKETS);

        // The control socket carries both our outbound sync packets and
        // the receiver's inbound resend requests — clone it so the sync
        // sender and the resend responder can use it concurrently.
        let control_for_resend = control_socket
            .try_clone()
            .context("try_clone control socket")?;

        // Anchor before audio: spawn_sync_sender sends the initial 0x90
        // sync synchronously before returning, so spawning it before the
        // audio sender guarantees the receiver has its rtptime→wall-clock
        // mapping before the first audio packet (iTunes: first sync 254 µs
        // before first audio; audio arriving before the anchor is
        // classified late and silently discarded).
        guard.adopt(
            spawn_sync_sender(
                control_socket,
                receiver_control_addr,
                current_rtptime.clone(),
                latency_samples,
                stop_flag.clone(),
                cfg.renderer.friendly_name.clone(),
            )
            .context("spawning AirPlay sync sender")?,
        );

        let sender_cfg = RtpSenderConfig {
            audio_socket: audio_socket_for_thread,
            receiver_addr: SocketAddr::new(cfg.renderer.ip, server_ports.audio),
            cipher,
            initial_seq,
            initial_rtptime,
            ssrc,
            samples_rx: cfg.samples_rx,
            stop_flag: stop_flag.clone(),
            receiver_name: cfg.renderer.friendly_name.clone(),
            current_rtptime: current_rtptime.clone(),
            resend: resend.clone(),
            uncompressed_alac: cfg.uncompressed_alac,
            session_dead: dead.clone(),
        };
        guard.adopt(spawn_audio_sender(sender_cfg)?);

        guard.adopt(
            spawn_resend_responder(
                control_for_resend,
                receiver_control_addr,
                resend,
                resend_stats.clone(),
                stop_flag.clone(),
                cfg.renderer.friendly_name.clone(),
            )
            .context("spawning AirPlay resend responder")?,
        );

        let rtsp = Arc::new(Mutex::new(rtsp));

        // RTSP keepalive — iTunes sends OPTIONS * every 2.0 s during
        // streaming (libraop uses 25 s; OwnTone SET_PARAMETER progress at
        // 25 s). Without one, nothing ever touches the control connection
        // after RECORD and the receiver eventually reaps the session
        // while our UI still says "streaming".
        let keepalive_handle = {
            let rtsp = rtsp.clone();
            let stop_flag = stop_flag.clone();
            let dead = dead.clone();
            let name = cfg.renderer.friendly_name.clone();
            std::thread::Builder::new()
                .name(format!("stream-to-speaker-airplay-keepalive:{}", name))
                .spawn(move || keepalive_loop(&rtsp, &stop_flag, &dead, &name, KEEPALIVE_CADENCE))
                .context("spawning AirPlay keepalive")?
        };
        guard.adopt(keepalive_handle);

        info!(
            "AirPlay: session up — {} ↔ {}:{} (RTSP), audio → {}:{}, control → :{}, timing → :{}",
            route.local,
            cfg.renderer.ip,
            cfg.renderer.port,
            cfg.renderer.ip,
            server_ports.audio,
            server_ports.control,
            server_ports.timing,
        );

        Ok(Self {
            renderer: cfg.renderer,
            rtsp,
            stop_flag,
            dead,
            resend_stats,
            current_rtptime,
            volume_pct: Arc::new(AtomicU32::new(cfg.initial_volume.unwrap_or(100))),
            threads: guard.into_threads(),
            abandon,
            opener,
            latency_samples,
            timeouts: cfg.timeouts,
            _audio_socket: audio_socket,
        })
    }

    /// A cheap, detachable handle for pushing track metadata without
    /// holding the (GUI-hot) session lock during the network send. The
    /// forwarder clones this under a brief session lock, releases the
    /// session lock, then calls [`MetadataHandle::send`] — which touches
    /// only the RTSP mutex (shared with the keepalive/volume, never the
    /// session mutex the GUI polls every repaint).
    ///
    /// None for a receiver that doesn't take track text (see
    /// [`AirPlayRenderer::wants_track_text`]).
    pub fn metadata_handle(&self) -> Option<MetadataHandle> {
        self.renderer.wants_track_text().then(|| MetadataHandle {
            rtsp: self.rtsp.clone(),
            current_rtptime: self.current_rtptime.clone(),
            latency_samples: self.latency_samples,
            timeouts: self.timeouts,
        })
    }

    /// The opener this session's connect used.
    pub fn opener(&self) -> Opener {
        self.opener
    }

    /// True once a background thread has flagged the session as dead
    /// (unreachable receiver / dropped connection). The app watchdog
    /// polls this to replace the zombie with a fresh session.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// `(resend requests, packets re-sent)` so far this session.
    pub fn resend_stats(&self) -> (u64, u64) {
        self.resend_stats.snapshot()
    }

    /// The level (0..=100) last set on the receiver, at first the one
    /// the session started it at.
    pub fn volume_pct(&self) -> u32 {
        self.volume_pct.load(Ordering::Relaxed)
    }

    /// Push a new volume value (0..=100) to the receiver. Idempotent
    /// across repeats; receivers throttle their own UI updates.
    pub fn set_volume_pct(&self, vol: u32) -> Result<()> {
        self.volume_control().set_volume_pct(vol)
    }

    /// Push a mute state. See [`VolumeHandle::set_mute`].
    pub fn set_mute(&self, muted: bool) -> Result<()> {
        self.volume_control().set_mute(muted)
    }

    /// A detachable handle for volume and mute, like
    /// [`Self::metadata_handle`]: the send touches only the RTSP mutex, so
    /// the caller can make it after releasing the session lock.
    pub fn volume_control(&self) -> VolumeHandle {
        VolumeHandle {
            rtsp: self.rtsp.clone(),
            volume_pct: self.volume_pct.clone(),
        }
    }

    /// Tear down: stop the background threads, TEARDOWN RTSP, close
    /// sockets. TEARDOWN matters: a receiver that isn't told keeps the
    /// session for a while and refuses or ignores the next connect (see
    /// [`RtspClient::teardown`]).
    pub fn stop(mut self) {
        info!(
            "AirPlay: stopping session to {}",
            self.renderer.friendly_name
        );
        self.stop_flag.store(true, Ordering::Release);
        // A request in flight (a keepalive, a volume change) normally
        // ends within milliseconds; one that doesn't is cut short, so
        // stopping never waits out its time limit. TEARDOWN then goes on
        // a new connection.
        let mut guard = match lock_within(&self.rtsp, STOP_GRACE) {
            Some(g) => g,
            None => {
                debug!("AirPlay: a request to {} is stalled; cutting it short", self.renderer.friendly_name);
                self.abandon.abandon();
                self.rtsp.lock().unwrap_or_else(|e| e.into_inner())
            }
        };
        // The keepalive only ever try-locks, so it can't block on the
        // lock held here.
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
        guard.teardown();
        note_left_session(&self.renderer.stable_id());
    }
}

impl Drop for AirPlaySession {
    fn drop(&mut self) {
        // Belt-and-braces: ensure the audio thread is told to stop if
        // someone drops a session without calling `stop`.
        self.stop_flag.store(true, Ordering::Release);
    }
}

/// Which kind of attempt: the opt-in MFi encryption experiment
/// (`/auth-setup` required, its secret wraps the audio key), or plaintext
/// / RSA with one of the openers.
#[derive(Clone, Copy, Debug)]
enum How {
    Mfi,
    Plain(Opener),
}

impl Opener {
    /// For log lines and error messages.
    fn describe(self) -> &'static str {
        match self {
            Opener::Options => "OPTIONS",
            Opener::AuthSetup => "/auth-setup",
            Opener::OptionsThenAuthSetup => "OPTIONS first",
        }
    }
}

/// A handshake that got through RECORD.
struct Handshake {
    rtsp: RtspClient,
    cipher: Cipher,
    ports: ServerPorts,
    latency: u32,
    opener: Opener,
}

/// An attempt that failed, and how far it got.
struct AttemptFailure {
    error: anyhow::Error,
    /// The TCP connection was made.
    connected: bool,
    /// RECORD was answered (with an error status).
    record_answered: bool,
    /// ANNOUNCE was accepted, so the receiver may have held a session
    /// (it has been sent TEARDOWN).
    announced: bool,
    /// The receiver answered 453.
    busy: bool,
}

impl AttemptFailure {
    fn new(error: anyhow::Error, connected: bool, announced: bool) -> Self {
        let busy = error.downcast_ref::<ReceiverBusy>().is_some();
        Self { error, connected, record_answered: false, announced, busy }
    }
}

/// What every attempt of one connect shares.
struct Bringup<'a> {
    cfg: &'a AirPlaySessionConfig,
    /// The address the first connection reached.
    route: Route,
    /// That connection, for the first attempt.
    first: std::cell::RefCell<Option<RtspClient>>,
    progress: &'a Progress,
    control_port: u16,
    timing_port: u16,
    initial_seq: u16,
    initial_rtptime: u32,
    target_latency: u32,
}

impl Bringup<'_> {
    /// One full RTSP bring-up on a new connection: connect → opener →
    /// ANNOUNCE → SETUP → RECORD. A failure after ANNOUNCE ends the
    /// receiver's session with TEARDOWN as the client is dropped.
    fn attempt(&self, how: How) -> Result<Handshake, AttemptFailure> {
        let cfg = self.cfg;
        let t = cfg.timeouts;
        let id = cfg.renderer.stable_id();
        let fail = |error: anyhow::Error, connected: bool| AttemptFailure::new(error, connected, false);
        // After ANNOUNCE was accepted: the session the receiver may hold
        // is torn down as `rtsp` drops, and noted as left.
        let fail_announced = |error: anyhow::Error| {
            note_left_session(&id);
            AttemptFailure::new(error, true, true)
        };
        let first = self.first.borrow_mut().take();
        let mut rtsp = match first {
            Some(c) => c,
            None => connect(cfg, self.route, self.progress).map_err(|e| fail(e, false))?,
        };
        let (cipher, opener) = self.open(&mut rtsp, how).map_err(|e| fail(e, true))?;

        let reply = t.reply;
        let progress = self.progress;
        progress.waiting(reply, || rtsp.announce(&cipher)).context("RTSP ANNOUNCE").map_err(|e| fail(e, true))?;
        let ports = progress
            .waiting(reply, || rtsp.setup(self.control_port, self.timing_port))
            .context("RTSP SETUP")
            .map_err(fail_announced)?;
        let record_latency = match progress.waiting(reply, || rtsp.record(self.initial_seq, self.initial_rtptime)) {
            Ok(l) => l,
            Err(e) => {
                // A status error means RECORD was answered; no reply at
                // all, or a broken connection, means it wasn't.
                let record_answered = e.downcast_ref::<RtspError>().is_none();
                let mut f = fail_announced(e.context("RTSP RECORD"));
                f.record_answered = record_answered;
                return Err(f);
            }
        };
        let advertised_latency = merge_advertised_latency(ports.audio_latency, record_latency);
        let latency = effective_latency_samples(advertised_latency, self.target_latency);
        info!(
            "AirPlay: sync anchor {} samples (~{} ms) — configured {} ms, receiver Audio-Latency: SETUP {:?} / RECORD {:?}",
            latency,
            latency as u64 * 1000 / crate::WIRE_SAMPLE_RATE as u64,
            cfg.latency_ms,
            ports.audio_latency,
            record_latency,
        );
        Ok(Handshake { rtsp, cipher, ports, latency, opener })
    }

    /// The opener, and the cipher it leads to. Once the receiver has
    /// answered the opener, later requests get the longer reply limit.
    fn open(&self, rtsp: &mut RtspClient, how: How) -> Result<(Cipher, Opener)> {
        let cfg = self.cfg;
        let t = cfg.timeouts;
        let progress = self.progress;
        let opener = match how {
            How::Mfi => {
                // iTunes encrypts audio to et=4 receivers with a key
                // wrapped under the auth-setup ECDH secret (mfiaeskey).
                let shared = progress
                    .waiting(t.opener, || rtsp.auth_setup())
                    .context("RTSP auth-setup (MFi opener)")?;
                rtsp.set_reply_limit(t.reply);
                return Ok((Cipher::Mfi(MfiKey::derive(&shared)), Opener::AuthSetup));
            }
            How::Plain(o) => o,
        };
        match opener {
            Opener::AuthSetup => {
                // Throwaway unlock — same as OwnTone/libraop: complete
                // the auth-setup exchange, ignore the secret, stream
                // plaintext. Field-proven recipe for Sonos-class
                // receivers.
                let _ = progress.waiting(t.opener, || rtsp.auth_setup()).context("RTSP auth-setup")?;
                rtsp.set_reply_limit(t.reply);
            }
            Opener::Options => {
                let reply = progress.waiting(t.opener, || rtsp.options()).context("RTSP OPTIONS")?;
                note_options_status(&cfg.renderer.friendly_name, &reply);
                rtsp.set_reply_limit(t.reply);
            }
            Opener::OptionsThenAuthSetup => {
                let reply = progress.waiting(t.opener, || rtsp.options_plain()).context("RTSP OPTIONS")?;
                note_options_status(&cfg.renderer.friendly_name, &reply);
                rtsp.set_reply_limit(t.reply);
                if reply.lists("POST") {
                    if let Err(e) = progress.waiting(t.reply, || rtsp.auth_setup()) {
                        if !rtsp.is_usable() {
                            return Err(e.context("RTSP auth-setup"));
                        }
                        warn!(
                            "AirPlay: {} didn't complete /auth-setup ({:#}); going on without it, as other senders do",
                            cfg.renderer.friendly_name, e
                        );
                    }
                }
            }
        }
        let cipher = if cfg.renderer.prefers_rsa_encryption() {
            // Classic AirPort Express (ek=1 / am=AirPort*) wants an
            // RSA-wrapped AES key; plaintext is silent there.
            Cipher::rsa()
        } else {
            Cipher::pick_for(&cfg.renderer.raop_encryption_types()).ok_or_else(|| {
                anyhow::anyhow!(
                    "no compatible encryption mode for {} (advertised et={:?})",
                    cfg.renderer.friendly_name,
                    cfg.renderer.encryption_types,
                )
            })?
        };
        Ok((cipher, opener))
    }
}

/// Open an RTSP connection to the receiver over `route`.
fn connect(cfg: &AirPlaySessionConfig, route: Route, progress: &Progress) -> Result<RtspClient> {
    let t = cfg.timeouts;
    // Password-protected receivers get RTSP Digest credentials;
    // the gen-1 AirPort Express (no `am` model) uses the iTunes
    // digest quirk (username "iTunes" + uppercase hex).
    let mut rtsp = progress
        .waiting(t.opener, || {
            RtspClient::connect_auth(
                route.receiver,
                cfg.renderer.port,
                route.local,
                t.opener,
                cfg.password.clone(),
                cfg.renderer.model.is_none(),
            )
        })
        .context("opening RTSP connection")?;
    rtsp.set_teardown_limit(t.teardown);
    Ok(rtsp)
}

/// The first connection of a connect: the receiver's addresses in
/// order, the next one tried while the TCP connection can't be made.
fn connect_first(cfg: &AirPlaySessionConfig, progress: &Progress) -> Result<(RtspClient, Route)> {
    let mut failed: Vec<String> = Vec::new();
    for &route in cfg.routes.iter().take(MAX_ROUTES) {
        match connect(cfg, route, progress) {
            Ok(c) => {
                if !failed.is_empty() {
                    info!("AirPlay: reached {} at {} ({})", cfg.renderer.friendly_name, route.receiver, failed.join("; "));
                }
                return Ok((c, route));
            }
            Err(e) => {
                warn!("AirPlay: couldn't reach {} at {} ({:#})", cfg.renderer.friendly_name, route.receiver, e);
                failed.push(format!("not at {}: {:#}", route.receiver, e));
            }
        }
    }
    if failed.is_empty() {
        anyhow::bail!("no address to reach {} at", cfg.renderer.friendly_name);
    }
    anyhow::bail!("{}", failed.join("; "))
}

/// Log an OPTIONS answer other than 200. The handshake goes on either
/// way, as it always has: receivers that don't implement OPTIONS answer
/// 501 and still stream.
fn note_options_status(name: &str, reply: &crate::airplay::rtsp::OptionsReply) {
    match reply.status {
        200 => {}
        403 => info!(
            "AirPlay: {} answered OPTIONS with 403 {}; it may want to be paired with a PIN first, which this app \
             doesn't do over AirPlay 1. Going on.",
            name, reply.reason
        ),
        code => info!("AirPlay: {} answered OPTIONS with {} {}; going on.", name, code, reply.reason),
    }
}

/// How often the keepalive runs: `every` while the receiver answers,
/// `after_failure` after an answer that said no, so a receiver that
/// refuses keepalives isn't asked every two seconds.
#[derive(Clone, Copy, Debug)]
struct KeepaliveCadence {
    every: Duration,
    after_failure: Duration,
}

const KEEPALIVE_CADENCE: KeepaliveCadence =
    KeepaliveCadence { every: Duration::from_secs(2), after_failure: Duration::from_secs(10) };

/// Send `OPTIONS *` on the session's connection until `stop_flag` is
/// set, and set `dead` (for the app's watchdog) once the session is
/// lost: the connection broke or the receiver closed it, a keepalive got
/// no answer within the reply limit, or two keepalives in a row were
/// answered with an error. A keepalive answered late but within the limit
/// keeps the session; a reply that comes after the limit is skipped by
/// its CSeq if the connection is used again (TEARDOWN).
fn keepalive_loop(
    rtsp: &Mutex<RtspClient>,
    stop_flag: &AtomicBool,
    dead: &AtomicBool,
    name: &str,
    cadence: KeepaliveCadence,
) {
    let mut refused = 0u32;
    loop {
        let interval = if refused == 0 { cadence.every } else { cadence.after_failure };
        if !sleep_unless_stopped(stop_flag, interval) {
            return;
        }
        // try_lock: skip the round rather than queue behind an in-flight
        // volume change — the next round covers it.
        let mut client = match rtsp.try_lock() {
            Ok(c) => c,
            Err(std::sync::TryLockError::WouldBlock) => continue,
            Err(std::sync::TryLockError::Poisoned(_)) => return,
        };
        if stop_flag.load(Ordering::Acquire) {
            return;
        }
        match client.options_keepalive() {
            Ok(()) => {
                if refused > 0 {
                    info!("AirPlay keepalive to {} answered again", name);
                }
                refused = 0;
            }
            Err(e) => {
                let no_answer = matches!(e.downcast_ref::<RtspError>(), Some(RtspError::NoAnswer { .. }));
                if no_answer || !client.is_usable() {
                    warn!("AirPlay: lost the session to {} ({:#})", name, e);
                    dead.store(true, Ordering::Release);
                    return;
                }
                refused += 1;
                warn!("AirPlay keepalive to {} refused ({:#})", name, e);
                if refused >= 2 {
                    warn!("AirPlay: lost the session to {} (two keepalives refused in a row)", name);
                    dead.store(true, Ordering::Release);
                    return;
                }
            }
        }
    }
}

/// Detached metadata sender — see [`AirPlaySession::metadata_handle`].
/// Holds only the shared RTSP connection + RTP clock, so the background
/// forwarder can send without contending on the session mutex.
pub struct MetadataHandle {
    rtsp: Arc<Mutex<RtspClient>>,
    current_rtptime: Arc<AtomicU32>,
    latency_samples: u32,
    timeouts: RaopTimeouts,
}

impl MetadataHandle {
    /// Push "now playing" to the receiver (best-effort, non-fatal),
    /// stamped with the position that is playing now. The reply is
    /// awaited only as long as the opener's limit; a receiver that never
    /// answers leaves the connection in step and the audio playing.
    pub fn send(&self, title: &str, artist: &str, album: &str) -> Result<()> {
        let body = crate::airplay::dmap::now_playing_body(title, artist, album);
        let rtptime = playing_rtptime(self.current_rtptime.load(Ordering::Acquire), self.latency_samples);
        let mut c = self.rtsp.lock().unwrap_or_else(|e| e.into_inner());
        c.set_reply_limit(self.timeouts.opener);
        let sent = c.set_metadata(&body, rtptime);
        c.set_reply_limit(self.timeouts.reply);
        sent
    }
}

/// The RTP time playing now: the sender's write head runs `latency`
/// samples ahead of what the receiver plays.
fn playing_rtptime(write_head: u32, latency: u32) -> u32 {
    write_head.wrapping_sub(latency)
}

/// Volume and mute for one RAOP session; see
/// [`AirPlaySession::volume_control`].
#[derive(Clone)]
pub struct VolumeHandle {
    rtsp: Arc<Mutex<RtspClient>>,
    volume_pct: Arc<AtomicU32>,
}

impl VolumeHandle {
    pub fn set_volume_pct(&self, vol: u32) -> Result<()> {
        self.volume_pct.store(vol.min(100), Ordering::Relaxed);
        let db = volume_pct_to_raop_db(vol);
        self.rtsp.lock().unwrap().set_volume(db)
    }

    /// Push a mute state. RAOP doesn't have a separate mute parameter,
    /// just the sentinel -144 dB "off" volume; an unmute restores the
    /// last level rather than jumping to full volume.
    pub fn set_mute(&self, muted: bool) -> Result<()> {
        let db = mute_db(muted, self.volume_pct.load(Ordering::Relaxed));
        self.rtsp.lock().unwrap().set_volume(db)
    }
}

/// Convert a 0..=100 percent volume to a RAOP-protocol dB value.
///
/// RAOP volume semantics:
///   *  0%   → -144.0 (sentinel "mute")
///   *  1%   → -30.0  (quietest non-muted)
///   * 100%  →   0.0  (loudest)
///
/// Interpolation between 1 and 100 is linear in dB. This matches
/// PulseAudio's RAOP sink and the iTunes reference.
pub fn volume_pct_to_raop_db(pct: u32) -> f32 {
    let pct = pct.min(100);
    if pct == 0 {
        return -144.0;
    }
    // Linear in dB from -30 (1%) to 0 (100%) over 99 steps.
    let pct_f = pct as f32;
    -30.0 + (pct_f - 1.0) * (30.0 / 99.0)
}

/// dB to send for a mute transition: the -144 sentinel when muting, the
/// last known level when unmuting. Windows mutes on its own when the
/// volume keys reach zero and unmutes on the next key up, so an unmute
/// that sent 0 dB (100 %) put the speaker at full volume while the
/// Windows slider still showed the old level.
pub fn mute_db(muted: bool, last_level_pct: u32) -> f32 {
    if muted {
        -144.0
    } else {
        volume_pct_to_raop_db(last_level_pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::fake_raop::{Answer, FakeRaop};
    use crate::airplay::raop_transcripts::{config, receiver};

    /// `stop()` doesn't wait behind a keepalive the receiver leaves
    /// unanswered: the stalled request is cut off, and TEARDOWN still
    /// goes out, on a new connection.
    #[test]
    fn stop_is_prompt_while_a_keepalive_is_unanswered() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.header("Apple-Challenge")) {
            // Keepalives are the OPTIONS without a challenge.
            ("OPTIONS", None) => Answer::Ignore,
            _ => Answer::Normal,
        });
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut cfg = config(receiver(&rx, "0B0000000002", &[0, 1]), samples);
        cfg.timeouts.reply = Duration::from_secs(20);
        let session = AirPlaySession::start(cfg).expect("session up");
        // The first keepalive goes out about 2 s in.
        assert!(rx.wait_until(Duration::from_secs(20), |ev| ev.iter().any(|e| matches!(e,
            crate::airplay::fake_raop::Event::Request(r) if r.method == "OPTIONS" && r.header("Apple-Challenge").is_none()))));
        let started = std::time::Instant::now();
        session.stop();
        let took = started.elapsed();
        assert!(took < Duration::from_secs(13), "stop took {:?}", took);
        assert!(rx.wait_until(Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e,
            crate::airplay::fake_raop::Event::Request(r) if r.method == "TEARDOWN"))), "{:?}", rx.lines());
    }

    /// A keepalive answered a little late doesn't end the session: the
    /// receiver gets as long to answer it as any request after the
    /// opener.
    #[test]
    fn a_keepalive_answered_late_does_not_end_the_session() {
        let delay = Duration::from_millis(200);
        let mut keepalives = 0;
        let rx = FakeRaop::start(move |req| match (req.method.as_str(), req.header("Apple-Challenge")) {
            ("OPTIONS", None) => {
                keepalives += 1;
                if keepalives == 1 {
                    Answer::late(delay, Answer::Normal)
                } else {
                    Answer::Normal
                }
            }
            _ => Answer::Normal,
        });
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut cfg = config(receiver(&rx, "0B0000000003", &[0, 1]), samples);
        cfg.timeouts = RaopTimeouts { opener: Duration::from_millis(100), reply: Duration::from_secs(2), ..RaopTimeouts::STANDARD };
        let session = AirPlaySession::start(cfg).expect("session up");
        let keepalive = |ev: &[crate::airplay::fake_raop::Event], n: usize| {
            ev.iter()
                .filter(|e| matches!(e, crate::airplay::fake_raop::Event::Request(r) if r.method == "OPTIONS" && r.header("Apple-Challenge").is_none()))
                .count()
                >= n
        };
        // Two more keepalives after the late one, each answered at once.
        assert!(rx.wait_until(Duration::from_secs(60), |ev| keepalive(ev, 3)), "{:?}", rx.lines());
        assert!(!session.is_dead(), "the late keepalive ended the session");
        session.stop();
    }

    /// A receiver that answers the opener at once but takes a while over
    /// SETUP is given the longer limit for it.
    #[test]
    fn setup_gets_the_longer_limit_once_the_opener_is_answered() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "SETUP" => Answer::late(Duration::from_millis(400), Answer::Normal),
            _ => Answer::Normal,
        });
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut cfg = config(receiver(&rx, "0B0000000004", &[0, 1]), samples);
        cfg.timeouts = RaopTimeouts { opener: Duration::from_millis(200), reply: Duration::from_secs(4), ..RaopTimeouts::STANDARD };
        let session = AirPlaySession::start(cfg).expect("SETUP answered within the longer limit");
        session.stop();
    }

    fn keepalive_client(rx: &FakeRaop) -> Mutex<RtspClient> {
        let ip = "127.0.0.1".parse().unwrap();
        Mutex::new(RtspClient::connect(ip, rx.port(), ip, Duration::from_millis(300)).unwrap())
    }

    const FAST: KeepaliveCadence = KeepaliveCadence { every: Duration::from_millis(100), after_failure: Duration::from_millis(100) };

    /// Run the keepalive until `rx` has seen `n` requests (or it ends by
    /// itself), then stop it; true if it flagged the session dead.
    fn run_keepalive(rx: &FakeRaop, n: usize) -> bool {
        let rtsp = keepalive_client(rx);
        let (stop, dead) = (AtomicBool::new(false), AtomicBool::new(false));
        std::thread::scope(|sc| {
            let h = sc.spawn(|| keepalive_loop(&rtsp, &stop, &dead, "test", FAST));
            let end = Instant::now() + Duration::from_secs(20);
            while !h.is_finished() && Instant::now() < end && !rx.wait_for_requests(n, Duration::from_millis(50)) {}
            stop.store(true, Ordering::Release);
        });
        dead.load(Ordering::Acquire)
    }

    #[test]
    fn keepalive_answered_or_501_keeps_the_session() {
        let mut n = 0;
        let rx = FakeRaop::start(move |_| {
            n += 1;
            if n % 2 == 0 { Answer::Reply(crate::airplay::fake_raop::Reply::status(501, "Not Implemented")) } else { Answer::Normal }
        });
        assert!(!run_keepalive(&rx, 4));
        assert!(rx.requests().len() >= 4);
    }

    #[test]
    fn keepalive_without_an_answer_loses_the_session() {
        let rx = FakeRaop::start(|_| Answer::Ignore);
        assert!(run_keepalive(&rx, 50));
        assert_eq!(rx.requests().len(), 1, "it stops once the session is lost");
    }

    #[test]
    fn keepalive_on_a_connection_the_receiver_closed_loses_the_session() {
        let rx = FakeRaop::start(|_| Answer::Close);
        assert!(run_keepalive(&rx, 50));
        assert_eq!(rx.requests().len(), 1);
    }

    #[test]
    fn one_refused_keepalive_is_forgiven_two_in_a_row_are_not() {
        let mut n = 0;
        let rx = FakeRaop::start(move |_| {
            n += 1;
            match n {
                2 => Answer::Reply(crate::airplay::fake_raop::Reply::status(500, "Error")),
                _ => Answer::Normal,
            }
        });
        assert!(!run_keepalive(&rx, 5));
        let rx = FakeRaop::start(|_| Answer::Reply(crate::airplay::fake_raop::Reply::status(454, "Session Not Found")));
        assert!(run_keepalive(&rx, 50));
        assert_eq!(rx.requests().len(), 2);
    }

    #[test]
    fn a_slow_connect_says_what_it_waits_for() {
        let progress = Progress::new();
        let lines = Mutex::new(Vec::<String>::new());
        let notice = |l: &str| lines.lock().unwrap().push(l.to_string());
        let slow_connect = Duration::from_secs(1);
        let slow_reply = Duration::from_millis(100);
        let started = Instant::now();
        std::thread::scope(|sc| {
            sc.spawn(|| progress.report_after(started, slow_connect, slow_reply, &notice));
            // A wait that ends quickly before the connect is slow: nothing.
            progress.waiting(Duration::from_secs(15), || std::thread::sleep(Duration::from_millis(100)));
            assert!(lines.lock().unwrap().is_empty());
            // Past the slow mark: a long wait is reported once, with the
            // time left.
            progress.waiting(Duration::from_secs(15), || {
                let end = Instant::now() + Duration::from_secs(10);
                while lines.lock().unwrap().is_empty() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::thread::sleep(Duration::from_millis(300));
            });
            progress.finish();
        });
        let lines = lines.into_inner().unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        // About a second of the 15 has gone by when it is reported.
        let left: u64 = lines[0]
            .strip_prefix("it hasn't answered yet; waiting up to ")
            .and_then(|r| r.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        assert!((12..=15).contains(&left), "{lines:?}");
    }

    use crate::airplay::fake_raop::Reply;

    /// Quick limits for tests of retries: the pause between attempts is
    /// measurable, the reply limit long against every delay used here.
    fn quick() -> RaopTimeouts {
        RaopTimeouts {
            opener: Duration::from_millis(300),
            reply: Duration::from_millis(600),
            teardown: Duration::from_millis(400),
            retry_pause: Duration::from_millis(200),
            busy_retry: Duration::from_millis(300),
        }
    }

    /// OPTIONS without POST in `Public`.
    fn options_without_post() -> Answer {
        Answer::Reply(Reply::ok().header("Public", "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER"))
    }

    fn start(rx: &FakeRaop, mac: &str, et: &[u8], hint: Option<Opener>) -> Result<AirPlaySession> {
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut cfg = config(receiver(rx, mac, et), samples);
        cfg.timeouts = quick();
        cfg.opener_hint = hint;
        AirPlaySession::start(cfg)
    }

    fn methods(rx: &FakeRaop) -> Vec<String> {
        rx.requests().iter().map(|r| format!("{} {}", r.conn, if r.method == "POST" { r.uri.as_str() } else { r.method.as_str() })).collect()
    }

    /// An MFi dock that refuses `/auth-setup` and whose OPTIONS doesn't
    /// list POST connects on the second attempt, opened the way OwnTone
    /// opens it.
    #[test]
    fn a_receiver_that_refuses_auth_setup_is_opened_the_reference_way() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.uri.as_str()) {
            ("POST", "/auth-setup") => Answer::Reply(Reply::status(404, "Not Found")),
            ("OPTIONS", _) => options_without_post(),
            _ => Answer::Normal,
        });
        let session = start(&rx, "0B0000000010", &[0, 4], None).expect("the second attempt connects");
        assert_eq!(session.opener(), Opener::OptionsThenAuthSetup);
        session.stop();
        assert_eq!(methods(&rx), ["0 /auth-setup", "1 OPTIONS", "1 ANNOUNCE", "1 SETUP", "1 RECORD", "1 SET_PARAMETER", "1 TEARDOWN"]);
        // The reference order's OPTIONS carries no challenge.
        assert_eq!(rx.requests()[1].header("Apple-Challenge"), None);
        // A pause between the two attempts.
        let first_end = rx.closed_at(0).expect("first connection closed");
        assert!(rx.connected_at(1).unwrap() >= first_end + quick().retry_pause - Duration::from_millis(50));
    }

    /// One that never answers `/auth-setup` but lists POST: the second
    /// attempt asks once more, gives up on it, and goes on to ANNOUNCE on
    /// the same connection.
    #[test]
    fn a_receiver_that_never_answers_auth_setup_is_opened_without_it() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.uri.as_str()) {
            ("POST", "/auth-setup") => Answer::Ignore,
            _ => Answer::Normal,
        });
        let session = start(&rx, "0B0000000011", &[0, 4], None).expect("connects without auth-setup");
        assert_eq!(session.opener(), Opener::OptionsThenAuthSetup);
        session.stop();
        assert_eq!(methods(&rx), ["0 /auth-setup", "1 OPTIONS", "1 /auth-setup", "1 ANNOUNCE", "1 SETUP", "1 RECORD", "1 SET_PARAMETER", "1 TEARDOWN"]);
    }

    /// A first attempt that fails after ANNOUNCE is ended with TEARDOWN
    /// before the second one starts.
    #[test]
    fn the_first_attempt_is_torn_down_before_the_second() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("SETUP", 0) => Answer::Ignore,
            _ => Answer::Normal,
        });
        let session = start(&rx, "0B0000000012", &[0, 4], None).expect("the second attempt connects");
        session.stop();
        let m = methods(&rx);
        assert_eq!(m[..4], ["0 /auth-setup", "0 ANNOUNCE", "0 SETUP", "0 TEARDOWN"]);
        assert_eq!(m[4..7], ["1 OPTIONS", "1 /auth-setup", "1 ANNOUNCE"]);
    }

    /// A receiver remembered as needing the reference order is opened that
    /// way first; if that fails, the usual way is tried next.
    #[test]
    fn a_remembered_opener_goes_first() {
        let rx = FakeRaop::start(|_| Answer::Normal);
        let session = start(&rx, "0B0000000013", &[0, 4], Some(Opener::OptionsThenAuthSetup)).expect("connects");
        assert_eq!(session.opener(), Opener::OptionsThenAuthSetup);
        session.stop();
        assert_eq!(methods(&rx)[..3], ["0 OPTIONS", "0 /auth-setup", "0 ANNOUNCE"]);

        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("OPTIONS", 0) => Answer::Close,
            _ => Answer::Normal,
        });
        let session = start(&rx, "0B0000000014", &[0, 4], Some(Opener::OptionsThenAuthSetup)).expect("the usual way connects");
        assert_eq!(session.opener(), Opener::AuthSetup);
        session.stop();
        assert_eq!(methods(&rx)[..3], ["0 OPTIONS", "1 /auth-setup", "1 ANNOUNCE"]);
    }

    #[test]
    fn only_the_reference_order_is_remembered() {
        assert_eq!(next_opener_memory(Some(Opener::OptionsThenAuthSetup)), Some(Opener::OptionsThenAuthSetup));
        assert_eq!(next_opener_memory(Some(Opener::AuthSetup)), None);
        assert_eq!(next_opener_memory(Some(Opener::Options)), None);
        assert_eq!(next_opener_memory(None), None);
    }

    /// Receivers that also offer AirPlay 2 (Sonos), plain ones, and an
    /// attempt that got as far as an answered RECORD get no second try.
    #[test]
    fn no_second_attempt_where_it_cannot_help() {
        // Sonos class: its fallback is AirPlay 2.
        let rx = FakeRaop::start(|req| match req.uri.as_str() {
            "/auth-setup" => Answer::Reply(Reply::status(404, "Not Found")),
            _ => Answer::Normal,
        });
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut r = receiver(&rx, "0B0000000015", &[0, 4]);
        r.features = Some(crate::airplay::discovery::FEAT_AUDIO | crate::airplay::discovery::FEAT_TRANSIENT_PAIRING);
        r.airplay_port = Some(7000);
        let mut cfg = config(r, samples);
        cfg.timeouts = quick();
        assert!(AirPlaySession::start(cfg).is_err());
        assert_eq!(methods(&rx), ["0 /auth-setup"]);

        // Plain receiver.
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "ANNOUNCE" => Answer::Reply(Reply::status(500, "Internal Server Error")),
            _ => Answer::Normal,
        });
        assert!(start(&rx, "0B0000000016", &[0, 1], None).is_err());
        assert_eq!(methods(&rx), ["0 OPTIONS", "0 ANNOUNCE"]);

        // RECORD answered with an error.
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "RECORD" => Answer::Reply(Reply::status(500, "Internal Server Error")),
            _ => Answer::Normal,
        });
        assert!(start(&rx, "0B0000000017", &[0, 4], None).is_err());
        assert!(rx.requests().iter().all(|r| r.conn == 0), "{:?}", rx.lines());

        // TCP connect refused: nothing to open.
        let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut r = receiver(&rx, "0B0000000018", &[0, 4]);
        r.port = port;
        let mut cfg = config(r, samples);
        // A pause long against everything else a failed start does
        // (stopping the timing thread takes up to half a second).
        cfg.timeouts = RaopTimeouts { retry_pause: Duration::from_secs(6), ..quick() };
        let started = Instant::now();
        assert!(AirPlaySession::start(cfg).is_err());
        assert!(started.elapsed() < Duration::from_secs(6), "no pause and no second attempt");
    }

    fn busy() -> Answer {
        Answer::Reply(Reply::status(453, "Not Enough Bandwidth"))
    }

    const BUSY_SENTENCE: &str = "Test Speaker is busy. Another device may be playing to it, or it hasn't let go of \
        this PC's last connection yet. Try again in a minute; if it stays busy, restart Test Speaker.";

    /// A receiver that answers 453 is reported busy in the app's own
    /// words, once: no retry when this app had no session on it, and no
    /// second opener (busy isn't about how the connect opens).
    #[test]
    fn a_busy_receiver_is_reported_in_plain_words() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "ANNOUNCE" => busy(),
            _ => Answer::Normal,
        });
        let err = start(&rx, "0B0000000020", &[0, 4], None).err().expect("busy");
        assert_eq!(format!("{:#}", err), BUSY_SENTENCE);
        assert!(err.downcast_ref::<Busy>().is_some());
        assert_eq!(methods(&rx), ["0 /auth-setup", "0 ANNOUNCE"]);
    }

    /// Soon after this app's own session on the receiver, a 453 is
    /// likely that session: one retry, after a pause.
    #[test]
    fn busy_soon_after_our_own_session_is_retried_once_after_a_pause() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("ANNOUNCE", 0) => busy(),
            _ => Answer::Normal,
        });
        let r = receiver(&rx, "0B0000000021", &[0, 1]);
        note_left_session(&r.stable_id());
        let session = start(&rx, "0B0000000021", &[0, 1], None).expect("the retry connects");
        session.stop();
        assert_eq!(methods(&rx)[..3], ["0 OPTIONS", "0 ANNOUNCE", "1 OPTIONS"]);
        let gap = rx.connected_at(1).unwrap().duration_since(rx.closed_at(0).unwrap());
        assert!(gap + Duration::from_millis(50) >= quick().busy_retry, "{:?}", gap);

        // Only once: a receiver that stays busy is reported busy.
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "ANNOUNCE" => busy(),
            _ => Answer::Normal,
        });
        note_left_session(&receiver(&rx, "0B0000000022", &[0, 1]).stable_id());
        let err = start(&rx, "0B0000000022", &[0, 1], None).err().expect("busy");
        assert_eq!(format!("{:#}", err), BUSY_SENTENCE);
        assert_eq!(methods(&rx), ["0 OPTIONS", "0 ANNOUNCE", "1 OPTIONS", "1 ANNOUNCE"]);
    }

    /// 453 to SETUP is busy too; that attempt's ANNOUNCE was accepted, so
    /// it is torn down and counts as this app's own session.
    #[test]
    fn busy_at_setup_tears_down_and_retries_once() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("SETUP", 0) => busy(),
            _ => Answer::Normal,
        });
        let session = start(&rx, "0B0000000023", &[0, 1], None).expect("the retry connects");
        session.stop();
        assert_eq!(methods(&rx)[..5], ["0 OPTIONS", "0 ANNOUNCE", "0 SETUP", "0 TEARDOWN", "1 OPTIONS"]);
    }

    /// A stopped session counts as this app's own for the next minute.
    #[test]
    fn a_stopped_session_makes_the_next_busy_answer_worth_one_retry() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("ANNOUNCE", 1) => busy(),
            _ => Answer::Normal,
        });
        start(&rx, "0B0000000024", &[0, 1], None).expect("first session").stop();
        start(&rx, "0B0000000024", &[0, 1], None).expect("second session, after one retry").stop();
        assert_eq!(rx.connected_at(3), None);
        assert!(rx.connected_at(2).is_some());
    }

    /// With the MFi experiment on, an MFi attempt that got past ANNOUNCE
    /// is torn down, and the plaintext attempt waits before it starts.
    #[test]
    fn the_mfi_experiment_pauses_before_plaintext_after_a_failed_announce() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("SETUP", 0) => Answer::Ignore,
            _ => Answer::Normal,
        });
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut cfg = config(receiver(&rx, "0B0000000025", &[0, 4]), samples);
        cfg.timeouts = quick();
        cfg.mfi_encryption = true;
        let session = AirPlaySession::start(cfg).expect("plaintext connects");
        session.stop();
        assert_eq!(methods(&rx)[..5], ["0 /auth-setup", "0 ANNOUNCE", "0 SETUP", "0 TEARDOWN", "1 /auth-setup"]);
        let gap = rx.connected_at(1).unwrap().duration_since(rx.closed_at(0).unwrap());
        assert!(gap + Duration::from_millis(50) >= quick().retry_pause, "{:?}", gap);
    }

    /// A receiver that can't be reached at its first address is tried at
    /// the next one, and the session uses the address that answered.
    #[test]
    fn a_connect_tries_the_next_address_when_the_first_refuses() {
        let rx = FakeRaop::start(|_| Answer::Normal);
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut cfg = config(receiver(&rx, "0B0000000030", &[0, 1]), samples);
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        // Nothing listens at 127.0.0.2 (the receiver is bound to 127.0.0.1).
        cfg.routes = vec![Route { receiver: "127.0.0.2".parse().unwrap(), local }, Route { receiver: local, local }];
        cfg.timeouts = quick();
        let session = AirPlaySession::start(cfg).expect("reached at the second address");
        assert_eq!(session.renderer.ip, local);
        session.stop();
        assert_eq!(rx.requests()[0].method, "OPTIONS");
    }

    /// Only addresses that can be a device's become routes; three at most.
    #[test]
    fn routes_leave_out_only_what_cannot_be_a_device() {
        let rx = FakeRaop::start(|_| Answer::Normal);
        let mut r = receiver(&rx, "0B0000000032", &[0, 1]);
        let lan = crate::discovery_net::LanTargets::from_networks([("192.168.1.10".parse().unwrap(), 24)]);
        let device = |a: IpAddr| crate::airplay::discovery::could_be_a_device(a, &lan);
        r.ip = "192.168.1.65".parse().unwrap();
        r.raop.addresses = ["192.168.1.65", "0.0.0.0", "224.0.0.251", "255.255.255.255", "192.168.1.255", "10.9.9.9", "127.0.0.1", "192.168.1.10"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        let local: IpAddr = "192.168.1.10".parse().unwrap();
        let routes = routes_for(&r, Route { receiver: r.ip, local }, device, |_| Some(local));
        let tried: Vec<String> = routes.iter().map(|r| r.receiver.to_string()).collect();
        assert_eq!(tried, ["192.168.1.65", "10.9.9.9", "127.0.0.1"]);
    }

    #[test]
    fn a_connect_tries_three_addresses_at_most() {
        let (_tx, samples) = crossbeam_channel::unbounded();
        let rx = FakeRaop::start(|_| Answer::Normal);
        let mut cfg = config(receiver(&rx, "0B0000000031", &[0, 1]), samples);
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let nowhere = |n: u8| Route { receiver: IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, n)), local };
        cfg.routes = vec![nowhere(2), nowhere(3), nowhere(4), Route { receiver: local, local }];
        cfg.timeouts = quick();
        let err = AirPlaySession::start(cfg).err().expect("the fourth address isn't tried");
        let msg = format!("{:#}", err);
        assert!(msg.contains("127.0.0.2") && msg.contains("127.0.0.3") && msg.contains("127.0.0.4"), "{msg}");
        assert!(rx.requests().is_empty());
    }

    /// A receiver that announces neither `cn` nor `et` connects in
    /// plaintext with the plain opener.
    #[test]
    fn a_receiver_without_codec_or_encryption_lists_connects_in_plaintext() {
        let rx = FakeRaop::start(|_| Answer::Normal);
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut r = receiver(&rx, "0B0000000040", &[]);
        r.codecs = vec![];
        let mut cfg = config(r, samples);
        cfg.timeouts = quick();
        let session = AirPlaySession::start(cfg).expect("connects");
        session.stop();
        let reqs = rx.requests();
        assert_eq!(methods(&rx)[..2], ["0 OPTIONS", "0 ANNOUNCE"]);
        let sdp = String::from_utf8(reqs[1].body.clone()).unwrap();
        assert!(!sdp.contains("aeskey") && !sdp.contains("aesiv"), "{sdp}");
    }

    #[test]
    fn track_text_is_stamped_with_what_plays_now() {
        assert_eq!(playing_rtptime(100_000, 88_200), 11_800);
        assert_eq!(playing_rtptime(5, 10), 5u32.wrapping_sub(10));
    }

    /// No track text for a receiver that didn't ask for it; one that asked
    /// and never answers keeps playing.
    #[test]
    fn track_text_goes_only_where_wanted_and_a_missing_answer_ends_nothing() {
        let rx = FakeRaop::start(|req| match req.header("Content-Type") {
            Some("application/x-dmap-tagged") => Answer::Ignore,
            _ => Answer::Normal,
        });
        let session = start(&rx, "0B0000000050", &[0, 1], None).expect("up");
        assert!(session.metadata_handle().is_none());
        session.stop();

        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut r = receiver(&rx, "0B0000000051", &[0, 1]);
        r.raop.metadata = vec![0, 1];
        let mut cfg = config(r, samples);
        cfg.timeouts = quick();
        let session = AirPlaySession::start(cfg).expect("up");
        let handle = session.metadata_handle().expect("it takes text");
        let started = Instant::now();
        assert!(handle.send("Song", "Artist", "Album").is_err(), "no answer");
        assert!(started.elapsed() < quick().reply * 10);
        // The connection is still in step: the next request is answered.
        session.set_volume_pct(30).expect("volume after the unanswered text");
        assert!(!session.is_dead());
        session.stop();
        let text = rx.requests().into_iter().find(|r| r.header("Content-Type") == Some("application/x-dmap-tagged")).unwrap();
        let record = rx.requests().into_iter().filter(|r| r.method == "RECORD").last().unwrap();
        let rtptime = |v: &str| v.rsplit("rtptime=").next().unwrap().parse::<u32>().unwrap();
        let start_rtp = rtptime(record.header("RTP-Info").unwrap());
        let stamped = rtptime(text.header("RTP-Info").unwrap());
        // About latency (88200) behind the write head, which is a little
        // past the start.
        let behind = start_rtp.wrapping_sub(stamped);
        assert!((44_100..=88_200).contains(&behind), "{behind}");
    }

    /// A receiver that asks for a password it never announced: the
    /// message names the speaker and says where to enter it, and no
    /// second opener is tried.
    #[test]
    fn a_password_request_is_reported_by_name_without_a_second_attempt() {
        let rx = FakeRaop::start(|_| {
            Answer::Reply(Reply::status(401, "Unauthorized").header("WWW-Authenticate", "Digest realm=\"raop\", nonce=\"1\""))
        });
        let err = start(&rx, "0B0000000060", &[0, 4], None).err().expect("refused");
        assert_eq!(
            format!("{:#}", err),
            "Test Speaker asks for its AirPlay password. Pick Test Speaker in the Stream To Speaker window to enter it."
        );
        assert_eq!(err.downcast_ref::<SignInNeeded>().map(|s| s.why.clone()), Some(SignIn::PasswordNeeded));
        assert_eq!(methods(&rx), ["0 /auth-setup"]);
    }

    /// An `et=4`-only receiver that refuses gets the same sentences as
    /// any other (here: busy), not an "unsupported" label; and it gets
    /// the second opener like `et=0,4`.
    #[test]
    fn an_mfi_only_receiver_gets_the_usual_handling() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "ANNOUNCE" => busy(),
            _ => Answer::Normal,
        });
        let err = start(&rx, "0B0000000070", &[4], None).err().expect("busy");
        assert_eq!(format!("{:#}", err), BUSY_SENTENCE);

        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.uri.as_str()) {
            ("POST", "/auth-setup") => Answer::Reply(Reply::status(404, "Not Found")),
            ("OPTIONS", _) => options_without_post(),
            _ => Answer::Normal,
        });
        let session = start(&rx, "0B0000000071", &[4], None).expect("the second opener connects");
        assert_eq!(session.opener(), Opener::OptionsThenAuthSetup);
        session.stop();
        let sdp = String::from_utf8(rx.requests().iter().find(|r| r.method == "ANNOUNCE").unwrap().body.clone()).unwrap();
        assert!(!sdp.contains("aeskey"), "unencrypted: {sdp}");
    }

    #[test]
    fn a_remembered_opener_reads_leniently() {
        #[derive(serde::Deserialize)]
        struct S {
            #[serde(default, deserialize_with = "deserialize_opener_lenient")]
            o: Option<Opener>,
        }
        let read = |j: &str| serde_json::from_str::<S>(j).unwrap().o;
        assert_eq!(read(r#"{"o":"options-then-auth-setup"}"#), Some(Opener::OptionsThenAuthSetup));
        assert_eq!(read(r#"{"o":"something-newer"}"#), None);
        assert_eq!(read(r#"{"o":7}"#), None);
        assert_eq!(read(r#"{"o":null}"#), None);
        assert_eq!(read(r#"{}"#), None);
    }

    #[test]
    fn pauses_say_why_once_the_connect_is_slow() {
        let progress = Progress::new();
        let lines = Mutex::new(Vec::<String>::new());
        let notice = |l: &str| lines.lock().unwrap().push(l.to_string());
        std::thread::scope(|sc| {
            sc.spawn(|| progress.report_after(Instant::now(), Duration::ZERO, Duration::from_secs(60), &notice));
            progress.pause(Pause::AnotherWay, Duration::from_millis(400));
            progress.pause(Pause::Busy, Duration::from_millis(400));
            progress.finish();
        });
        assert_eq!(
            lines.into_inner().unwrap(),
            ["the first try didn't work; trying once more another way.", "it is busy; trying once more."]
        );
    }

    /// A bring-up that fails after ANNOUNCE tells the receiver, so it
    /// doesn't keep the session and refuse the next attempt.
    #[test]
    fn a_bring_up_that_fails_after_announce_sends_teardown() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "SETUP" => Answer::Ignore,
            _ => Answer::Normal,
        });
        let (_tx, samples) = crossbeam_channel::unbounded();
        let mut cfg = config(receiver(&rx, "0B0000000001", &[0, 4]), samples);
        cfg.timeouts.reply = Duration::from_millis(300);
        assert!(AirPlaySession::start(cfg).is_err());
        assert!(rx.wait_for_requests(4, Duration::from_secs(10)), "{:?}", rx.lines());
        let lines = rx.lines();
        assert_eq!(lines[..3], ["0 POST /auth-setup".to_string(), lines[1].clone(), lines[2].clone()]);
        assert!(lines[1].starts_with("0 ANNOUNCE ") && lines[2].starts_with("0 SETUP "));
        assert!(lines[3].starts_with("0 TEARDOWN "), "{:?}", lines);
    }

    #[test]
    fn volume_mapping_anchor_points() {
        assert_eq!(volume_pct_to_raop_db(0), -144.0);
        // Unmute restores the level; it never jumps to 0 dB unless the
        // level really is 100.
        assert_eq!(mute_db(true, 30), -144.0);
        assert_eq!(mute_db(false, 30), volume_pct_to_raop_db(30));
        assert!(mute_db(false, 30) < -20.0);
        assert_eq!(mute_db(false, 100), 0.0);
        assert_eq!(mute_db(false, 0), -144.0);
        assert!((volume_pct_to_raop_db(1) - -30.0).abs() < 1e-4);
        assert!((volume_pct_to_raop_db(100) - 0.0).abs() < 1e-4);
        // Monotonic
        let mut prev = volume_pct_to_raop_db(1);
        for p in 2..=100 {
            let v = volume_pct_to_raop_db(p);
            assert!(v > prev, "non-monotonic at {}: {} → {}", p, prev, v);
            prev = v;
        }
    }

    #[test]
    fn latency_anchor_defaults_and_honors_larger() {
        let d = DEFAULT_LATENCY_SAMPLES;
        // No Audio-Latency header (Sonos, most speakers) → the proven
        // 88200 anchor, byte-identical to the old constant.
        assert_eq!(effective_latency_samples(None, d), 88200);
        // A small advertised value never lowers the floor.
        assert_eq!(effective_latency_samples(Some(0), d), 88200);
        assert_eq!(effective_latency_samples(Some(11025), d), 88200);
        assert_eq!(effective_latency_samples(Some(88200), d), 88200);
        // A big-DSP AVR asking for a deeper buffer gets it.
        assert_eq!(effective_latency_samples(Some(132300), d), 132300);
    }

    #[test]
    fn latency_anchor_low_target_honors_receiver_floor() {
        // Low-latency mode: a 100 ms target is 4410 samples.
        let target = crate::airplay::timing::latency_ms_to_samples(100);
        assert_eq!(target, 4410);
        // Silent receiver, or one content with less → the target wins.
        assert_eq!(effective_latency_samples(None, target), 4410);
        assert_eq!(effective_latency_samples(Some(220), target), 4410);
        // ...but never below what the receiver says it needs.
        assert_eq!(effective_latency_samples(Some(11025), target), 11025);
        // And the default target still maps to the iTunes constant.
        assert_eq!(crate::airplay::timing::latency_ms_to_samples(2000), 88200);
    }

    #[test]
    fn advertised_latency_merges_setup_and_record() {
        assert_eq!(merge_advertised_latency(None, None), None);
        assert_eq!(merge_advertised_latency(Some(220), None), Some(220));
        assert_eq!(merge_advertised_latency(None, Some(11025)), Some(11025));
        // Both stated: the deeper floor wins whichever leg carried it.
        assert_eq!(merge_advertised_latency(Some(220), Some(11025)), Some(11025));
        assert_eq!(merge_advertised_latency(Some(132300), Some(11025)), Some(132300));
    }
}
