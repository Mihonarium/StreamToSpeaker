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
use crate::airplay::rtsp::{AbandonHandle, RtspClient, RtspError, ServerPorts, TEARDOWN_LIMIT};
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
    /// Local IPv4 we'll bind UDP sockets to and advertise in SDP: this
    /// PC's address on the receiver's network (or `--advertise-ip`).
    pub local_ip: IpAddr,
    /// Subscription on the StreamHub — produces PcmFrame entries the
    /// audio thread consumes.
    pub samples_rx: Receiver<PcmFrame>,
    /// Initial volume in [0, 100] — converted to RAOP dB internally.
    pub initial_volume: Option<u32>,
    /// Time limits for the connect and the session's requests; the app
    /// uses [`RaopTimeouts::STANDARD`].
    pub timeouts: RaopTimeouts,
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
}

impl RaopTimeouts {
    pub const STANDARD: Self = Self {
        opener: Duration::from_secs(3),
        reply: Duration::from_secs(15),
        teardown: TEARDOWN_LIMIT,
    };
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
            if let (Stage::Waiting { limit }, since) = (st.0, st.1) {
                let waited = now.saturating_duration_since(since);
                if now >= started + slow_connect && waited >= slow_reply && reported != Some(since) {
                    reported = Some(since);
                    let left = limit.saturating_sub(waited).as_secs_f32().ceil() as u64;
                    let line = match left {
                        0 | 1 => "it hasn't answered yet; waiting a moment longer.".to_string(),
                        n => format!("it hasn't answered yet; waiting up to {} more seconds.", n),
                    };
                    drop(st);
                    notice(&line);
                    st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    continue;
                }
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

    /// [`start`](Self::start), passing `notice` a line (to follow "Still
    /// connecting to <name> — ") whenever a connect that has already run
    /// [`SLOW_CONNECT`] is waiting on the receiver, so the window can say
    /// what the wait is for.
    pub fn start_with_notices(cfg: AirPlaySessionConfig, notice: &(dyn Fn(&str) + Sync)) -> Result<Self> {
        let progress = Progress::new();
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| progress.report(started, notice));
            let out = Self::start_inner(cfg, &progress);
            progress.finish();
            out
        })
    }

    fn start_inner(cfg: AirPlaySessionConfig, progress: &Progress) -> Result<Self> {
        info!(
            "AirPlay: starting session to {} ({}:{})",
            cfg.renderer.friendly_name, cfg.renderer.ip, cfg.renderer.port,
        );

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

        // Bind the three UDP sockets first so we know our ports for SETUP.
        let audio_socket = bind_udp(cfg.local_ip).context("bind audio UDP")?;
        let control_socket = bind_udp(cfg.local_ip).context("bind control UDP")?;
        let timing_socket = bind_udp(cfg.local_ip).context("bind timing UDP")?;
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
        let mfi_style = want_mfi || cfg.renderer.supports_airplay2();
        // The et=4 attempt is opt-in: the wrap is a best-grounded guess,
        // and a stalled attempt wedges this receiver class for tens of
        // seconds (capture-proven: after one SETUP stall the Sonos
        // answers zero RTSP bytes on ANY connection for the hold window),
        // so it must never gate default connectivity.
        let try_mfi_first = cfg.mfi_encryption && want_mfi;

        // One full RTSP bring-up (connect → opener → ANNOUNCE → SETUP →
        // RECORD). Factored so the opt-in MFi experiment can fall back to
        // the proven plaintext/RSA recipe.
        //
        // Opener: iTunes → Sonos (AirTunes/366) leads with POST /auth-setup
        // (0x01 + X25519), not OPTIONS + Apple-Challenge. MFi/AP2 devices
        // get auth-setup; plain legacy receivers keep OPTIONS.
        let target_latency_samples = crate::airplay::timing::latency_ms_to_samples(cfg.latency_ms);
        let timeouts = cfg.timeouts;
        let attempt = |try_mfi: bool| -> Result<(RtspClient, Cipher, ServerPorts, u32)> {
            // Password-protected receivers get RTSP Digest credentials;
            // the gen-1 AirPort Express (no `am` model) uses the iTunes
            // digest quirk (username "iTunes" + uppercase hex).
            let mut rtsp = progress.waiting(timeouts.opener, || {
                RtspClient::connect_auth(
                    cfg.renderer.ip,
                    cfg.renderer.port,
                    cfg.local_ip,
                    timeouts.opener,
                    cfg.password.clone(),
                    cfg.renderer.model.is_none(),
                )
            })
            .context("opening RTSP connection")?;
            rtsp.set_teardown_limit(timeouts.teardown);

            let cipher = if try_mfi {
                // iTunes encrypts audio to et=4 receivers with a key
                // wrapped under the auth-setup ECDH secret (mfiaeskey).
                let shared = progress
                    .waiting(timeouts.opener, || rtsp.auth_setup())
                    .context("RTSP auth-setup (MFi opener)")?;
                rtsp.set_reply_limit(timeouts.reply);
                Cipher::Mfi(MfiKey::derive(&shared))
            } else {
                if mfi_style {
                    // Throwaway unlock — same as OwnTone/libraop: complete
                    // the auth-setup exchange, ignore the secret, stream
                    // plaintext. Field-proven recipe for Sonos-class
                    // receivers.
                    let _ = progress.waiting(timeouts.opener, || rtsp.auth_setup()).context("RTSP auth-setup")?;
                } else {
                    progress.waiting(timeouts.opener, || rtsp.options()).context("RTSP OPTIONS")?;
                }
                // The receiver has answered: it is there, and the rest of
                // the handshake may take it longer.
                rtsp.set_reply_limit(timeouts.reply);
                if cfg.renderer.prefers_rsa_encryption() {
                    // Classic AirPort Express (ek=1 / am=AirPort*) wants an
                    // RSA-wrapped AES key; plaintext is silent there.
                    Cipher::rsa()
                } else {
                    Cipher::pick_for(&cfg.renderer.encryption_types).ok_or_else(|| {
                        anyhow::anyhow!(
                            "no compatible encryption mode for {} (advertised et={:?})",
                            cfg.renderer.friendly_name,
                            cfg.renderer.encryption_types,
                        )
                    })?
                }
            };

            let reply = timeouts.reply;
            progress.waiting(reply, || rtsp.announce(&cipher)).context("RTSP ANNOUNCE")?;
            let ports = progress.waiting(reply, || rtsp.setup(control_port, timing_port)).context("RTSP SETUP")?;
            let record_latency = progress
                .waiting(reply, || rtsp.record(initial_seq, initial_rtptime))
                .context("RTSP RECORD")?;
            let advertised_latency = merge_advertised_latency(ports.audio_latency, record_latency);
            let latency = effective_latency_samples(advertised_latency, target_latency_samples);
            info!(
                "AirPlay: sync anchor {} samples (~{} ms) — configured {} ms, receiver Audio-Latency: SETUP {:?} / RECORD {:?}",
                latency,
                latency as u64 * 1000 / crate::WIRE_SAMPLE_RATE as u64,
                cfg.latency_ms,
                ports.audio_latency,
                record_latency,
            );
            Ok((rtsp, cipher, ports, latency))
        };

        // On any error below, `guard`'s Drop stops and joins whatever
        // threads exist — the timing responder now, the sync/audio/resend
        // threads once adopted.
        let handshake = if try_mfi_first {
            match attempt(true) {
                Ok(v) => Ok(v),
                Err(e) => {
                    warn!(
                        "AirPlay: et=4 MFi handshake to {} failed ({:#}); \
                         retrying with plaintext/RSA",
                        cfg.renderer.friendly_name, e
                    );
                    attempt(false).context("plaintext fallback after MFi handshake failed")
                }
            }
        } else {
            attempt(false)
        };
        let (mut rtsp, cipher, server_ports, latency_samples) = handshake?;
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
            cfg.local_ip,
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
            _audio_socket: audio_socket,
        })
    }

    /// A cheap, detachable handle for pushing track metadata without
    /// holding the (GUI-hot) session lock during the network send. The
    /// forwarder clones this under a brief session lock, releases the
    /// session lock, then calls [`MetadataHandle::send`] — which touches
    /// only the RTSP mutex (shared with the keepalive/volume, never the
    /// session mutex the GUI polls every repaint).
    pub fn metadata_handle(&self) -> MetadataHandle {
        MetadataHandle {
            rtsp: self.rtsp.clone(),
            current_rtptime: self.current_rtptime.clone(),
        }
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
    }
}

impl Drop for AirPlaySession {
    fn drop(&mut self) {
        // Belt-and-braces: ensure the audio thread is told to stop if
        // someone drops a session without calling `stop`.
        self.stop_flag.store(true, Ordering::Release);
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
}

impl MetadataHandle {
    /// Push "now playing" to the receiver (best-effort, non-fatal).
    pub fn send(&self, title: &str, artist: &str, album: &str) -> Result<()> {
        let body = crate::airplay::dmap::now_playing_body(title, artist, album);
        let rtptime = self.current_rtptime.load(Ordering::Acquire);
        self.rtsp.lock().unwrap().set_metadata(&body, rtptime)
    }
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
        cfg.timeouts = RaopTimeouts { opener: Duration::from_millis(100), reply: Duration::from_secs(2), teardown: TEARDOWN_LIMIT };
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
        cfg.timeouts = RaopTimeouts { opener: Duration::from_millis(200), reply: Duration::from_secs(4), teardown: TEARDOWN_LIMIT };
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
