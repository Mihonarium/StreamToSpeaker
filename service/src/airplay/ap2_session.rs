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
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::airplay::alac::build_uncompressed_alac_frame;
use crate::airplay::ap2_crypto::AudioSealer;
use crate::airplay::ap2_events::spawn_event_channel;
use crate::airplay::ap2_health::{
    log_notice, raop_db_to_volume_pct, AnchorAction, AnchorFailure, AnchorRetry, Ap2Fault, DeviceVolume,
    FaultChannel, FaultSlot, FeedbackAction, FeedbackMonitor, SendAction, SendHealth, VolumeTracker,
    FEEDBACK_INTERVAL,
};
use crate::airplay::ap2_ptp::{
    spawn_ptp_master, spawn_ptp_master_on, PortsProblem, PtpMaster, PtpPorts, PtpTimeline, PTP_PORTS,
};
use crate::airplay::ap2_resend::Retransmitter;
use crate::airplay::ap2_crypto::ChannelCipher;
use crate::airplay::ap2_realtime::{MemberLink, RealtimeGroup, RealtimeRun};
use crate::airplay::ap2_rtsp::{Ap2Rtsp, InfoPoll, InfoVolume, PairVerifyError, RealtimeCodec, TransientOutcome};
use crate::airplay::discovery::AirPlayRenderer;
use crate::airplay::hap_pairing::StoredPairing;
use crate::airplay::rtp::{bind_udp, random_initial_rtptime, random_initial_seq, random_ssrc, FRAMES_PER_PACKET};
use crate::airplay::session::{mute_db, volume_pct_to_raop_db};
use crate::airplay::timing::{
    spawn_resend_responder, spawn_sync_sender, spawn_sync_sender_ptp, spawn_timing_responder, ResendBuffer,
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
/// How far in the future the buffered stream's SETRATEANCHORTIME anchor is
/// placed — the receiver buffers packets until this point, absorbing
/// startup jitter.
const ANCHOR_LEAD_NS: u64 = 500_000_000;
/// Recently-sent packets retained for retransmit (~4 s at 44.1 kHz).
const RESEND_BUFFER_PACKETS: usize = 512;
/// Device-volume read-back cadence (`GET /info`).
const VOLUME_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Response budget for one read-back request — short, so a slow receiver
/// never holds the shared control connection for long.
const VOLUME_POLL_TIMEOUT: Duration = Duration::from_millis(1000);

pub struct AirPlay2SessionConfig {
    pub renderer: AirPlayRenderer,
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
    /// This receiver's stored HomeKit pairing, if it was PIN-paired earlier
    /// (Apple TV with access control), keys already parsed. When present,
    /// the session pair-verifies with it and never falls back to transient
    /// pairing.
    pub stored_pairing: Option<StoredPairing>,
    /// Stream HomePods with the low-latency realtime profile (see
    /// [`AirPlay2Session::start`]); `false` keeps single HomePods on the
    /// general path (buffered when supported). Stereo pairs always use it.
    pub homepod_realtime: bool,
    /// Playout delay for the HomePod realtime profile, in ms.
    pub homepod_latency_ms: u32,
}

/// Typed outcome of [`AirPlay2Session::start`], so the caller's policy
/// decisions (launch the PIN ceremony? which words?) are compiler-checked
/// matches instead of string sniffing or fragile anyhow downcasts that
/// silently break under a future `.map_err` re-wrap.
///
/// No outcome is grounds for deleting the stored pairing: no refusal is
/// authenticated (see [`PairVerifyError`]). A new PIN pairing replaces it,
/// and only the user removes it.
#[derive(Debug, thiserror::Error)]
pub enum Ap2StartError {
    /// The receiver refused transient pairing (HTTP 470) and no stored
    /// credentials exist: it needs a one-time PIN ceremony (Apple TV with
    /// access control / "Require Device Verification").
    #[error("receiver requires one-time PIN pairing")]
    NeedsPin,
    /// Pair-verify with the stored pairing got an answer from something
    /// that never proved it holds the paired accessory's key: another
    /// device at the address, an impostor, or the accessory reset to new
    /// keys.
    #[error("the receiver didn't prove it is the paired device: {0:#}")]
    PairingNotProven(#[source] anyhow::Error),
    /// The receiver proved it is the paired accessory, then refused this
    /// PC's pairing (it no longer knows it — removed on the device, or a
    /// reset); possibly forged by a relay.
    #[error("the receiver no longer accepts this PC's pairing: {0:#}")]
    PairingRefused(#[source] anyhow::Error),
    /// Everything else (connect failures, SETUP refusals, timeouts, a
    /// pair-verify that got no answer, …).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Volume and mute for one AirPlay 2 session (every pair member); see
/// [`AirPlay2Session::volume_control`].
#[derive(Clone)]
pub struct VolumeHandle {
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    followers: Vec<Arc<Mutex<Ap2Rtsp>>>,
    volume_pct: Arc<AtomicU32>,
    volume: Arc<Mutex<VolumeTracker>>,
}

impl VolumeHandle {
    pub fn set_volume_pct(&self, vol: u32) -> Result<()> {
        self.volume_pct.store(vol.min(100), Ordering::Relaxed);
        let seq = self.volume.lock().unwrap().on_write(vol, Instant::now());
        let res = self.set_volume_db_all(volume_pct_to_raop_db(vol));
        if res.is_err() {
            self.volume.lock().unwrap().on_write_failed(seq);
        }
        res
    }

    pub fn set_mute(&self, muted: bool) -> Result<()> {
        let db = mute_db(muted, self.volume_pct.load(Ordering::Relaxed));
        self.set_volume_db_all(db)
    }

    /// Write the volume to every member, leader first. Every member is
    /// tried; the first failure is returned.
    fn set_volume_db_all(&self, db: f32) -> Result<()> {
        let mut result = self.rtsp.lock().unwrap().set_volume(db);
        for f in &self.followers {
            let r = f.lock().unwrap().set_volume(db);
            if result.is_ok() {
                result = r;
            }
        }
        result
    }
}

/// A live AirPlay 2 session.
pub struct AirPlay2Session {
    pub renderer: AirPlayRenderer,
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    /// Last volume (0..=100) pushed to the receiver; restored on unmute.
    volume_pct: Arc<AtomicU32>,
    stop_flag: Arc<AtomicBool>,
    /// Set by background threads when the session has demonstrably died
    /// (see [`FaultSlot`]: audio sends failing for the grace period, a
    /// buffered TCP write failure, a /feedback fault, the event channel
    /// closing). Polled by the app watchdog for auto-reconnect.
    dead: Arc<AtomicBool>,
    /// Why the session died, once a thread has raised a fault.
    faults: FaultSlot,
    /// Device-volume read-back state.
    volume: Arc<Mutex<VolumeTracker>>,
    /// Retransmission counters (realtime stream only; buffered has no
    /// resend path and leaves them at zero).
    resend_stats: Arc<crate::airplay::timing::ResendStats>,
    threads: SessionThreads,
    ptp_session: Option<PtpMaster>,
    /// (next 23-bit seq, next rtptime) for the buffered stream — read at
    /// stop() to send the spec's FLUSHBUFFERED before TEARDOWN. None for
    /// realtime sessions.
    buffered_flush: Option<(Arc<AtomicU32>, Arc<AtomicU32>)>,
    /// Clone of the buffered data TCP stream — stop() shuts it down to
    /// unblock a sender wedged in a full-buffer write before joining it.
    data_stream: Option<TcpStream>,
    _audio_socket: Option<UdpSocket>,
    /// Control connections of the other stereo-pair members (the leader's
    /// is `rtsp`). Volume goes to the leader first, then these.
    followers: Vec<Arc<Mutex<Ap2Rtsp>>>,
}

impl AirPlay2Session {
    /// Bring up a session. HomePods with PTP (and every stereo pair) take
    /// the realtime HomePod profile ([`AirPlay2Session::start_homepod_realtime`])
    /// unless `homepod_realtime` is off; every other receiver takes the
    /// general path below.
    pub fn start(cfg: AirPlay2SessionConfig) -> std::result::Result<Self, Ap2StartError> {
        if bring_up_route(&cfg, aac_encoder_available) == BringUpRoute::HomePodRealtime {
            return Self::start_homepod_realtime(cfg);
        }
        let port = cfg.renderer.airplay_port.unwrap_or(DEFAULT_AIRPLAY_PORT);
        info!(
            "AirPlay 2: starting session to {} ({}:{})",
            cfg.renderer.friendly_name, cfg.renderer.ip, port
        );

        // UDP sockets for audio (out), control (sync out), timing (responder).
        let audio_socket = bind_udp(cfg.local_ip).context("bind AP2 audio UDP")?;
        let control_socket = bind_udp(cfg.local_ip).context("bind AP2 control UDP")?;
        let timing_socket = bind_udp(cfg.local_ip).context("bind AP2 timing UDP")?;
        let control_port = control_socket.local_addr().context("AP2 control socket addr")?.port();
        let timing_port = timing_socket.local_addr().context("AP2 timing socket addr")?.port();

        let mut rtsp = Ap2Rtsp::connect(cfg.renderer.ip, port, cfg.local_ip, Duration::from_secs(5))
            .context("AirPlay 2 RTSP connect")?;
        // HomePods get the stricter connection handling (framing checks,
        // short TEARDOWN wait); other receivers keep the lenient one.
        let homepod = cfg.renderer.is_homepod();
        rtsp.set_strict(homepod);

        // Canonical opener — iOS sends GET /info before pairing. Some
        // receivers initialise per-connection state on it; harmless
        // everywhere else, so failure is non-fatal.
        if let Err(e) = rtsp.get_info() {
            warn!("AirPlay 2 GET /info failed (continuing): {:#}", e);
        }

        let audio_key = pair_member(&mut rtsp, cfg.stored_pairing.as_ref(), &cfg.renderer.friendly_name)?;
        info!("AirPlay 2: paired with {}", cfg.renderer.friendly_name);
        let sockets = Ap2Sockets { audio: audio_socket, control: control_socket, timing: timing_socket, control_port, timing_port };
        Self::start_paired(cfg, rtsp, audio_key, sockets, BringUpEnv::real())
    }

    /// The general path's bring-up after pairing: timing, the event
    /// channel, the stream, RECORD and the session's threads.
    fn start_paired(
        cfg: AirPlay2SessionConfig,
        mut rtsp: Ap2Rtsp,
        audio_key: [u8; 32],
        sockets: Ap2Sockets,
        env: BringUpEnv,
    ) -> std::result::Result<Self, Ap2StartError> {
        let Ap2Sockets { audio: audio_socket, control: control_socket, timing: timing_socket, control_port, timing_port } =
            sockets;
        let homepod = cfg.renderer.is_homepod();
        let event_ciphers = rtsp.take_event_ciphers();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let faults = FaultSlot::new(dead.clone(), cfg.renderer.friendly_name.clone()).with_key(cfg.renderer.stable_id());
        let mut bring_up = BringUp::new(&stop_flag, &faults);
        let resend_stats = Arc::new(crate::airplay::timing::ResendStats::default());
        // Shared from RECORD on (the buffered sender anchors on it, the
        // feedback keepalive posts to it, volume changes arrive from other
        // threads); held by the bring-up until then.
        let rtsp = Arc::new(Mutex::new(rtsp));
        bring_up.hold(&rtsp);
        let mut conn = rtsp.lock().unwrap();

        // Timing-protocol choice: receivers advertising SupportsPTP (bit 41)
        // get the full PTP path — field-tested on a SYMFONISK whose current
        // firmware stalls the stream SETUP under timingProtocol=NTP (NTP
        // appears as vestigial as its RAOP). For PTP **we are the
        // grandmaster** — start the master first so its clock identity goes
        // into the SETUP payload and the clock (plus Announce/Signaling) is
        // already being served when the receiver processes it.
        let use_ptp = cfg.renderer.expects_ptp();
        info!(
            "AirPlay 2: timing for {} = {} (model {:?})",
            cfg.renderer.friendly_name,
            if use_ptp { "PTP (we serve as grandmaster)" } else { "NTP" },
            cfg.renderer.model.as_deref().unwrap_or("?"),
        );
        let timing_setup = if use_ptp {
            let ptp = bring_up.ptp.insert(
                spawn_ptp_master_on(
                    &[cfg.renderer.ip],
                    cfg.local_ip,
                    env.ptp_ports,
                    cfg.renderer.friendly_name.clone(),
                    homepod,
                )
                .context("starting AP2 PTP master")?,
            );
            conn.setup_timing_ptp(ptp.timeline.clock_id, &ptp.clock_uuid).context("AP2 SETUP(timing/PTP)")?
        } else {
            conn.setup_timing_ntp(timing_port).context("AP2 SETUP(timing/NTP)")?
        };
        let event_port = timing_setup.event_port;

        // Open the event channel: the receiver withholds its RECORD response
        // until the sender has a TCP connection to its eventPort. On a
        // HomePod its requests are decrypted and answered 200, and the
        // receiver closing it ends the session; other receivers keep the
        // channel open and drained, as before.
        bring_up.threads.event = spawn_event_channel(
            cfg.renderer.ip,
            event_port,
            event_ciphers.filter(|_| homepod),
            stop_flag.clone(),
            faults.clone(),
            homepod,
            cfg.renderer.friendly_name.clone(),
        );

        // Stream kind: buffered (type 103, TCP — what iOS actually uses,
        // and seemingly the only kind current Sonos firmware truly plays)
        // when the receiver advertises bit 40 and we're on PTP; realtime
        // (type 96, UDP) otherwise. The buffered codec is AAC-LC via the
        // Windows-provided Media Foundation encoder — iOS's buffered codec,
        // the only one field-proven on Sonos. Without that encoder (Windows
        // N editions without the Media Feature Pack) the session is the
        // realtime one a person who chose realtime gets; a refused buffered
        // SETUP falls back to realtime too. Every rejection is visible.
        let latency_samples = crate::airplay::timing::latency_ms_to_samples(cfg.latency_ms);
        let low_latency = cfg.latency_ms < LOW_LATENCY_REALTIME_MS;
        let mut want_buffered = wants_buffered(&cfg);
        if cfg.prefer_realtime {
            info!("AirPlay 2: prefer_realtime_airplay set — using the realtime stream");
        } else if low_latency && use_ptp && cfg.renderer.supports_buffered_audio() {
            info!(
                "AirPlay 2: airplay_latency_ms={} is below {} ms — using the realtime stream \
                 (buffered holds seconds regardless)",
                cfg.latency_ms, LOW_LATENCY_REALTIME_MS
            );
        }
        if want_buffered {
            if let Err(e) = (env.aac_encoder)() {
                info!(
                    "AirPlay 2: the buffered stream needs Windows' AAC encoder, which is not available \
                     ({e:#}); using the realtime stream instead"
                );
                want_buffered = false;
            }
        }
        let (ports, buffered) = if want_buffered {
            match conn.setup_stream_buffered(&audio_key, control_port, 4, BUFFERED_SPF as u64, 0x400000, DEFAULT_LATENCY_SAMPLES) {
                Ok(p) => {
                    info!("AirPlay 2: buffered stream accepted (type 103/AAC-LC, TCP data port {})", p.data);
                    (p, true)
                }
                Err(e) => {
                    warn!("AirPlay 2: buffered SETUP rejected ({e:#}); falling back to realtime");
                    let p = conn
                        .setup_stream(&audio_key, control_port, latency_samples)
                        .context("AP2 SETUP(stream, realtime fallback)")?;
                    (p, false)
                }
            }
        } else {
            let p = conn
                .setup_stream(&audio_key, control_port, latency_samples)
                .context("AP2 SETUP(stream)")?;
            (p, false)
        };
        debug!("AirPlay 2: receiver data port {}, control port {}", ports.data, ports.control);

        // Open the buffered data connection straight after the stream SETUP
        // — before SETPEERS/RECORD. Receivers hold connection state per
        // phase (the event channel must exist before RECORD, the anchor
        // only works at first audio), and real senders connect the data
        // socket early too.
        let mut data_stream: Option<TcpStream> = None;
        let mut early_data: Option<TcpStream> = None;
        if buffered {
            let data_addr = SocketAddr::new(cfg.renderer.ip, ports.data);
            let stream = TcpStream::connect_timeout(&data_addr, Duration::from_secs(3))
                .with_context(|| format!("connecting AP2 buffered data TCP to {}", data_addr))?;
            stream.set_nodelay(true).ok();
            // Without a write timeout, a receiver that stops consuming
            // fills the socket buffer and write_all blocks forever —
            // which wedges stop() on the sender join and leaves the
            // whole app stuck "Connecting…" with TEARDOWN never sent.
            stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
            data_stream = stream.try_clone().ok();
            early_data = Some(stream);
        }

        if use_ptp {
            // Hand the receiver the PTP peer address list. OwnTone's order:
            // receiver's address first, then the sender's.
            if let Err(e) = conn.set_peers(&[cfg.renderer.ip, cfg.local_ip]) {
                warn!("AirPlay 2 SETPEERS failed (PTP may not lock): {}", e);
            }
        }

        conn.record().context("AP2 RECORD")?;

        let volume = Arc::new(Mutex::new(VolumeTracker::default()));
        if let Some(vol) = cfg.initial_volume {
            let seq = volume.lock().unwrap().on_write(vol, Instant::now());
            if let Err(e) = conn.set_volume(volume_pct_to_raop_db(vol)) {
                warn!("AirPlay 2 initial volume failed: {}", e);
                volume.lock().unwrap().on_write_failed(seq);
            }
        }

        // From here the RTSP connection is shared: the buffered sender
        // anchors on it at first audio, the feedback keepalive posts to it,
        // and volume changes arrive from arbitrary threads.
        drop(conn);

        // Background threads.
        let initial_seq = random_initial_seq();
        let initial_rtptime = random_initial_rtptime();
        let ssrc = random_ssrc();
        let current_rtptime = Arc::new(AtomicU32::new(initial_rtptime));

        // The control socket carries outbound sync packets and inbound
        // resend requests — clone it so both threads can use it.
        let control_for_resend = control_socket
            .try_clone()
            .context("clone AP2 control socket")?;

        let mut buffered_flush = None;
        if buffered {
            // Buffered: audio goes over the (already-connected) TCP data
            // connection; playback is anchored by SETRATEANCHORTIME. No
            // sync packets, no resend (TCP is reliable), no NTP timing
            // responder.
            let stream = early_data.take().expect("buffered implies early data connection");
            let last_seq = Arc::new(AtomicU32::new(initial_seq as u32));
            buffered_flush = Some((last_seq.clone(), current_rtptime.clone()));
            // The anchor (SETRATEANCHORTIME) is sent by the sender thread
            // itself, right before the first real audio packet — anchoring
            // earlier maps an rtpTime to a wall instant that has passed by
            // the time audio exists, and the receiver drops everything as
            // late. Field-tested: this receiver accepts anchors only on
            // ITS OWN timeline, which the PTP layer follows via the
            // receiver's Sync/Follow_Up stream.
            //
            // Nothing after this point can fail the bring-up, and the sender's
            // encoder can fail as soon as its thread starts: its faults count
            // for the repeat rule from here.
            faults.arm(Instant::now());
            bring_up.threads.sender = Some(spawn_ap2_buffered_sender(BufferedSenderConfig {
                stream,
                audio_key,
                initial_seq,
                initial_rtptime,
                samples_rx: cfg.samples_rx,
                stop_flag: stop_flag.clone(),
                receiver_name: cfg.renderer.friendly_name.clone(),
                current_rtptime: current_rtptime.clone(),
                last_seq,
                rtsp: rtsp.clone(),
                timeline: bring_up.ptp.as_ref().expect("buffered implies PTP").timeline.clone(),
                encoder: (env.buffered_encoder)(),
                anchor_retry: AnchorRetry::new(),
                homepod,
                faults: faults.clone(),
            })?);
            (env.after_sender_spawn)();
            info!("AirPlay 2: buffered AAC-LC stream armed — will anchor at first audio");
            drop(timing_socket);
            drop(control_socket);
            drop(control_for_resend);
        } else {
            // Anchor before audio: both sync spawners send their initial
            // (extension-bit) sync synchronously before returning, so
            // spawning them BEFORE the audio sender guarantees the
            // receiver has a timeline anchor before the first audio
            // packet arrives — same ordering rule as the RAOP path.
            let sync_addr = SocketAddr::new(cfg.renderer.ip, ports.control);
            if use_ptp {
                let ptp = bring_up.ptp.as_ref().expect("use_ptp started the PTP master");
                let sync = spawn_sync_sender_ptp(
                    control_socket,
                    sync_addr,
                    current_rtptime.clone(),
                    latency_samples,
                    ptp.timeline.clone(),
                    stop_flag.clone(),
                    cfg.renderer.friendly_name.clone(),
                )
                .context("spawning AP2 PTP sync sender")?;
                bring_up.threads.sync = Some(sync);
                // The NTP timing responder is unused under PTP; release its socket.
                drop(timing_socket);
            } else {
                let timing = spawn_timing_responder(
                    timing_socket,
                    stop_flag.clone(),
                    cfg.renderer.friendly_name.clone(),
                )
                .context("spawning AP2 timing responder")?;
                bring_up.threads.timing = Some(timing);
                let sync = spawn_sync_sender(
                    control_socket,
                    sync_addr,
                    current_rtptime.clone(),
                    latency_samples,
                    stop_flag.clone(),
                    cfg.renderer.friendly_name.clone(),
                )
                .context("spawning AP2 sync sender")?;
                bring_up.threads.sync = Some(sync);
            }

            // HomePods: resends from the media loop with per-slot budgets
            // and playout deadlines, and a grace period for send errors.
            // The resend socket gets a 1 ms *read* timeout rather than
            // non-blocking mode, which would also apply to the sync
            // thread's sends on the shared socket. Other receivers keep
            // the resend thread and stop on the first send error.
            let recovery = if homepod {
                control_for_resend
                    .set_read_timeout(Some(Duration::from_millis(1)))
                    .context("AP2 control socket read timeout")?;
                let recovery = RealtimeRecovery::Deadline {
                    control_socket: control_for_resend,
                    retransmit: Retransmitter::new(
                        cfg.renderer.ip,
                        resend_stats.clone(),
                        cfg.renderer.friendly_name.clone(),
                    ),
                    latency: Duration::from_millis(cfg.latency_ms as u64),
                };
                recovery
            } else {
                let resend = ResendBuffer::new(RESEND_BUFFER_PACKETS);
                let handle = spawn_resend_responder(
                    control_for_resend,
                    sync_addr,
                    resend.clone(),
                    resend_stats.clone(),
                    stop_flag.clone(),
                    cfg.renderer.friendly_name.clone(),
                )
                .context("spawning AP2 resend responder")?;
                bring_up.threads.resend = Some(handle);
                RealtimeRecovery::Classic { resend }
            };
            bring_up.threads.sender = Some(spawn_ap2_sender(Ap2SenderConfig {
                audio_socket: audio_socket.try_clone().context("clone AP2 audio socket")?,
                receiver_addr: SocketAddr::new(cfg.renderer.ip, ports.data),
                audio_key,
                initial_seq,
                initial_rtptime,
                ssrc,
                samples_rx: cfg.samples_rx,
                stop_flag: stop_flag.clone(),
                receiver_name: cfg.renderer.friendly_name.clone(),
                current_rtptime,
                recovery,
                faults: faults.clone(),
            })?);
        }

        info!(
            "AirPlay 2: session up — {} → {}:{} ({} audio), control :{}",
            cfg.renderer.friendly_name,
            cfg.renderer.ip,
            ports.data,
            if buffered { "buffered/TCP" } else { "realtime/UDP" },
            control_port,
        );

        // /feedback keepalive — iOS senders POST this every ~2 s; some
        // receivers eventually drop (or never fully start) sessions
        // without it. Shares the RTSP connection via the session mutex.
        bring_up.threads.feedback = spawn_feedback_keepalive(
            rtsp.clone(),
            stop_flag.clone(),
            faults.clone(),
            homepod,
            cfg.renderer.friendly_name.clone(),
        );
        // Volume read-back is limited to HomePods.
        if homepod {
            bring_up.threads.volume =
                spawn_volume_reader(rtsp.clone(), volume.clone(), stop_flag.clone(), cfg.renderer.friendly_name.clone());
        }

        let (threads, ptp_session) = bring_up.finish();
        Ok(Self {
            renderer: cfg.renderer,
            rtsp,
            volume_pct: Arc::new(AtomicU32::new(cfg.initial_volume.unwrap_or(100))),
            stop_flag,
            dead,
            faults,
            volume,
            resend_stats,
            threads,
            ptp_session,
            buffered_flush,
            data_stream,
            _audio_socket: Some(audio_socket),
            followers: Vec::new(),
        })
    }

    /// The realtime HomePod profile, for a single HomePod or every member
    /// of a stereo pair (one session per member, leader first):
    ///
    /// 1. per member: connect, `GET /info`, pair;
    /// 2. one PTP grandmaster shared by all members;
    /// 3. per member: SETUP(session) with the multi-select keys, then the
    ///    encrypted event channel;
    /// 4. per member: SETPEERS listing every member plus us;
    /// 5. per member: SETUP(stream) type 96, PCM when every member's `cn`
    ///    allows it (else ALAC), `latencyMin` = `latencyMax` = the
    ///    configured delay, `streamConnectionID` = its SSRC; a reported
    ///    `latencyMin` becomes that member's effective latency;
    /// 6. per member: RECORD + FLUSH naming its first packet, all on one
    ///    shared initial RTP timestamp;
    /// 7. one realtime scheduler for all members, `/feedback` and the event
    ///    channel per member, volume read-back from the leader.
    ///
    /// Any member's fault ends the whole session. A failure partway tears
    /// down what was set up (TEARDOWN on drop, threads stopped).
    fn start_homepod_realtime(cfg: AirPlay2SessionConfig) -> std::result::Result<Self, Ap2StartError> {
        let label = cfg.renderer.friendly_name.clone();
        let members: Vec<AirPlayRenderer> = match &cfg.renderer.pair {
            Some(p) if !p.is_complete() => {
                return Err(anyhow::anyhow!(
                    "{}: waiting for every member of the stereo pair ({} of {} present)",
                    label,
                    p.members.len(),
                    p.expected
                )
                .into())
            }
            Some(p) => p.members.clone(),
            None => vec![cfg.renderer.clone()],
        };
        let codec = common_codec(&members).ok_or_else(|| {
            anyhow::anyhow!("{}: receiver offers neither PCM nor ALAC (cn={:?})", label, members[0].codecs)
        })?;
        let latency_samples = crate::airplay::timing::latency_ms_to_samples(cfg.homepod_latency_ms);
        info!(
            "AirPlay 2: starting HomePod realtime session to {} ({} member(s), {} ms, {})",
            label,
            members.len(),
            cfg.homepod_latency_ms,
            codec.label()
        );

        let stop_flag = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let faults = FaultSlot::new(dead.clone(), label.clone()).with_key(cfg.renderer.stable_id());
        let mut bring_up = BringUp::new(&stop_flag, &faults);
        let resend_stats = Arc::new(crate::airplay::timing::ResendStats::default());

        struct Pending {
            renderer: AirPlayRenderer,
            rtsp: Arc<Mutex<Ap2Rtsp>>,
            audio_key: [u8; 32],
            event_ciphers: Option<(ChannelCipher, ChannelCipher)>,
            audio_socket: Option<UdpSocket>,
            control_socket: Option<UdpSocket>,
        }

        // 1. Connect + pair, leader first.
        let mut pending: Vec<Pending> = Vec::with_capacity(members.len());
        for r in &members {
            let audio_socket = bind_udp(cfg.local_ip).context("bind AP2 audio UDP")?;
            let control_socket = bind_udp(cfg.local_ip).context("bind AP2 control UDP")?;
            let port = r.airplay_port.unwrap_or(DEFAULT_AIRPLAY_PORT);
            let mut rtsp = Ap2Rtsp::connect(r.ip, port, cfg.local_ip, Duration::from_secs(3))
                .with_context(|| format!("AirPlay 2 RTSP connect to {}", r.friendly_name))?;
            rtsp.set_strict(true);
            if let Err(e) = rtsp.get_info() {
                warn!("AirPlay 2 GET /info to {} failed (continuing): {:#}", r.friendly_name, e);
            }
            // Stored PIN credentials are keyed by the selected entry, so
            // they only apply to a single receiver.
            let audio_key = if members.len() == 1 {
                pair_member(&mut rtsp, cfg.stored_pairing.as_ref(), &r.friendly_name)?
            } else {
                // A PIN ceremony pairs one receiver under the selected
                // entry's id, so it can't serve a pair: report it plainly
                // instead of prompting for a PIN that would never be used.
                match pair_member(&mut rtsp, None, &r.friendly_name) {
                    Ok(k) => k,
                    Err(Ap2StartError::NeedsPin) => {
                        return Err(anyhow::anyhow!(
                            "{}: this stereo-pair member requires PIN pairing, which pairs don't support",
                            r.friendly_name
                        )
                        .into())
                    }
                    Err(e) => return Err(e),
                }
            };
            info!("AirPlay 2: paired with {}", r.friendly_name);
            let event_ciphers = rtsp.take_event_ciphers();
            let rtsp = Arc::new(Mutex::new(rtsp));
            bring_up.hold(&rtsp);
            pending.push(Pending {
                renderer: r.clone(),
                rtsp,
                audio_key,
                event_ciphers,
                audio_socket: Some(audio_socket),
                control_socket: Some(control_socket),
            });
        }

        // 2. One grandmaster for every member.
        let member_ips: Vec<IpAddr> = members.iter().map(|r| r.ip).collect();
        let ptp = bring_up
            .ptp
            .insert(spawn_ptp_master(&member_ips, cfg.local_ip, label.clone(), true).context("starting AP2 PTP master")?);
        let (ptp_clock_id, ptp_clock_uuid, ptp_clock) = (ptp.timeline.clock_id, ptp.clock_uuid.clone(), ptp.timeline.clock.clone());

        // 3. Session SETUP + event channel.
        for p in &mut pending {
            let ts = p
                .rtsp
                .lock()
                .unwrap()
                .setup_timing_ptp_homepod(ptp_clock_id, &ptp_clock_uuid, crate::PRODUCT_NAME)
                .with_context(|| format!("AP2 SETUP(session) to {}", p.renderer.friendly_name))?;
            if ts.event_port == 0 {
                return Err(anyhow::anyhow!("{}: SETUP response has no eventPort", p.renderer.friendly_name).into());
            }
            if let Some(h) = spawn_event_channel(
                p.renderer.ip,
                ts.event_port,
                p.event_ciphers.take(),
                stop_flag.clone(),
                faults.clone(),
                true,
                p.renderer.friendly_name.clone(),
            ) {
                bring_up.threads.extra.push(h);
            }
        }

        // 4. Every member gets the same peer list: all members, then us.
        let mut peers = member_ips.clone();
        peers.push(cfg.local_ip);
        for p in &mut pending {
            if let Err(e) = p.rtsp.lock().unwrap().set_peers(&peers) {
                warn!("AirPlay 2 SETPEERS to {} failed (PTP may not lock): {:#}", p.renderer.friendly_name, e);
            }
        }

        // 5. Stream SETUP: shared initial timestamp, per-member seq/SSRC.
        let initial_rtptime = random_initial_rtptime();
        let mut links: Vec<MemberLink> = Vec::with_capacity(pending.len());
        for p in &mut pending {
            let control_socket = p.control_socket.take().expect("bound above");
            let audio_socket = p.audio_socket.take().expect("bound above");
            let control_port = control_socket.local_addr().context("AP2 control socket addr")?.port();
            let ssrc = random_ssrc();
            let first_seq = random_initial_seq();
            let (ports, reported) = p
                .rtsp
                .lock()
                .unwrap()
                .setup_stream_pinned(&p.audio_key, control_port, codec, latency_samples, ssrc)
                .with_context(|| format!("AP2 SETUP(stream) to {}", p.renderer.friendly_name))?;
            let effective = match reported {
                Some(v) => {
                    info!("AirPlay 2 {}: receiver latency {} samples (reported)", p.renderer.friendly_name, v);
                    v
                }
                None => {
                    info!(
                        "AirPlay 2 {}: receiver latency {} samples (estimated — not reported)",
                        p.renderer.friendly_name, latency_samples
                    );
                    latency_samples
                }
            };
            control_socket
                .set_nonblocking(true)
                .context("AP2 control socket non-blocking")?;
            links.push(MemberLink {
                name: p.renderer.friendly_name.clone(),
                audio_socket,
                data_addr: SocketAddr::new(p.renderer.ip, ports.data),
                control_socket,
                control_addr: SocketAddr::new(p.renderer.ip, ports.control),
                sealer: AudioSealer::new(&p.audio_key, first_seq),
                seq: first_seq,
                ssrc,
                latency_samples: effective,
                retransmit: Retransmitter::new(p.renderer.ip, resend_stats.clone(), p.renderer.friendly_name.clone()),
                health: SendHealth::new(Instant::now()),
                packets: 0,
            });
        }

        // 6. RECORD + FLUSH naming each member's first packet.
        for (p, l) in pending.iter_mut().zip(&links) {
            p.rtsp
                .lock()
                .unwrap()
                .record_and_flush_at(l.seq, initial_rtptime)
                .with_context(|| format!("AP2 RECORD to {}", p.renderer.friendly_name))?;
        }

        // Initial volume to every member, leader first.
        let volume = Arc::new(Mutex::new(VolumeTracker::default()));
        if let Some(vol) = cfg.initial_volume {
            let seq = volume.lock().unwrap().on_write(vol, Instant::now());
            for p in &mut pending {
                if let Err(e) = p.rtsp.lock().unwrap().set_volume(volume_pct_to_raop_db(vol)) {
                    warn!("AirPlay 2 initial volume to {} failed: {}", p.renderer.friendly_name, e);
                    volume.lock().unwrap().on_write_failed(seq);
                }
            }
        }

        // 7. Media, keepalives, read-back.
        let group = RealtimeGroup::new(links, initial_rtptime, ptp_clock, ptp_clock_id);
        bring_up.threads.sender = Some(crate::airplay::ap2_realtime::spawn(RealtimeRun {
            group,
            codec,
            samples_rx: cfg.samples_rx,
            stop_flag: stop_flag.clone(),
            faults: faults.clone(),
            name: label.clone(),
        })
        .context("spawning AP2 realtime sender")?);

        let mut conns = pending
            .into_iter()
            .map(|p| (p.renderer.friendly_name, p.rtsp));
        let (leader_name, rtsp) = conns.next().expect("at least one member");
        let followers: Vec<(String, Arc<Mutex<Ap2Rtsp>>)> = conns.collect();
        bring_up.threads.feedback =
            spawn_feedback_keepalive(rtsp.clone(), stop_flag.clone(), faults.clone(), true, leader_name.clone());
        for (name, conn) in &followers {
            bring_up.threads.extra.extend(spawn_feedback_keepalive(
                conn.clone(),
                stop_flag.clone(),
                faults.clone(),
                true,
                name.clone(),
            ));
        }
        bring_up.threads.volume = spawn_volume_reader(rtsp.clone(), volume.clone(), stop_flag.clone(), leader_name);

        info!("AirPlay 2: HomePod realtime session up — {}", label);
        let (threads, ptp_session) = bring_up.finish();
        Ok(Self {
            renderer: cfg.renderer,
            rtsp,
            volume_pct: Arc::new(AtomicU32::new(cfg.initial_volume.unwrap_or(100))),
            stop_flag,
            dead,
            faults,
            volume,
            resend_stats,
            threads,
            ptp_session,
            buffered_flush: None,
            data_stream: None,
            _audio_socket: None,
            followers: followers.into_iter().map(|(_, c)| c).collect(),
        })
    }

    /// True once a background thread flagged the session dead (dropped
    /// receiver). Polled by the app watchdog for auto-reconnect.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Whether the session runs the buffered stream, whose delay is its
    /// own (fixed) rather than the one asked for.
    pub fn is_buffered(&self) -> bool {
        self.buffered_flush.is_some()
    }

    /// The fault that ended the session, if one was raised. A
    /// non-retryable fault means reconnecting won't help.
    pub fn fault(&self) -> Option<Ap2Fault> {
        self.faults.get()
    }

    /// `(resend requests, packets re-sent)` so far this session.
    pub fn resend_stats(&self) -> (u64, u64) {
        self.resend_stats.snapshot()
    }

    /// The receiver's own volume as last read back, with its sync state.
    pub fn device_volume(&self) -> DeviceVolume {
        self.volume.lock().unwrap().state(Instant::now())
    }

    /// The level (0..=100) last set on the receiver, at first the one
    /// the session started it at.
    pub fn volume_pct(&self) -> u32 {
        self.volume_pct.load(Ordering::Relaxed)
    }

    pub fn set_volume_pct(&self, vol: u32) -> Result<()> {
        self.volume_control().set_volume_pct(vol)
    }

    pub fn set_mute(&self, muted: bool) -> Result<()> {
        self.volume_control().set_mute(muted)
    }

    /// A detachable handle for volume and mute: the send touches only the
    /// members' RTSP mutexes and the read-back tracker, so the caller can
    /// make it after releasing the session lock.
    pub fn volume_control(&self) -> VolumeHandle {
        VolumeHandle {
            rtsp: self.rtsp.clone(),
            followers: self.followers.clone(),
            volume_pct: self.volume_pct.clone(),
            volume: self.volume.clone(),
        }
    }

    pub fn stop(mut self) {
        info!("AirPlay 2: stopping session to {}", self.renderer.friendly_name);
        self.stop_flag.store(true, Ordering::Release);
        // Unblock a buffered sender wedged in a full-buffer TCP write —
        // without this the join below can hang forever on a receiver
        // that stopped consuming, freezing the whole switch-speaker flow.
        if let Some(s) = &self.data_stream {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        self.threads.join();
        for f in &self.followers {
            f.lock().unwrap().teardown();
        }
        if let Some(ptp) = self.ptp_session.take() {
            ptp.stop();
        }
        // Buffered sessions get the spec's FLUSHBUFFERED before TEARDOWN
        // so the receiver drops its buffered tail instead of playing it out.
        if let Some((seq, ts)) = self.buffered_flush.take() {
            let mut guard = self.rtsp.lock().unwrap();
            if let Err(e) = guard.flush_buffered(seq.load(Ordering::Acquire), ts.load(Ordering::Acquire)) {
                debug!("AirPlay 2 FLUSHBUFFERED failed (continuing to TEARDOWN): {:#}", e);
            }
            guard.teardown();
            return;
        }
        self.rtsp.lock().unwrap().teardown();
    }
}

/// Which bring-up a session takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BringUpRoute {
    /// [`AirPlay2Session::start_homepod_realtime`].
    HomePodRealtime,
    /// The general path ([`AirPlay2Session::start_paired`] after pairing).
    General,
}

/// The HomePod realtime profile for a stereo pair and for a HomePod with
/// PTP on its default setting; also for a HomePod set to the buffered
/// stream when Windows' AAC encoder can't be made (`aac_encoder`), since
/// that profile is what HomePods get by default. The general path
/// otherwise.
fn bring_up_route(cfg: &AirPlay2SessionConfig, aac_encoder: fn() -> Result<()>) -> BringUpRoute {
    let r = &cfg.renderer;
    if r.pair.is_some() || (cfg.homepod_realtime && r.is_homepod() && r.expects_ptp()) {
        return BringUpRoute::HomePodRealtime;
    }
    if r.is_homepod() && wants_buffered(cfg) {
        if let Err(e) = aac_encoder() {
            info!(
                "AirPlay 2: the buffered stream needs Windows' AAC encoder, which is not available \
                 ({e:#}); using the HomePod realtime profile for {} instead",
                r.friendly_name
            );
            return BringUpRoute::HomePodRealtime;
        }
    }
    BringUpRoute::General
}

/// The general path would offer the buffered stream: the receiver takes it
/// (bit 40) on PTP, and the person chose neither realtime nor a latency
/// below what buffered holds.
fn wants_buffered(cfg: &AirPlay2SessionConfig) -> bool {
    cfg.renderer.expects_ptp()
        && cfg.renderer.supports_buffered_audio()
        && !cfg.prefer_realtime
        && cfg.latency_ms >= LOW_LATENCY_REALTIME_MS
}

/// What a general-path bring-up takes from the machine: the PTP ports and
/// whether Windows' AAC encoder can be made here. Tests replace both.
struct BringUpEnv {
    ptp_ports: PtpPorts,
    aac_encoder: fn() -> Result<()>,
    /// What makes the buffered sender's encoder, on its thread.
    buffered_encoder: fn() -> EncoderFactory,
    /// Runs right after the buffered sender is spawned (nothing in the app;
    /// tests stand in for a bring-up that is slow to finish).
    after_sender_spawn: fn(),
}

impl BringUpEnv {
    fn real() -> Self {
        Self {
            ptp_ports: PTP_PORTS,
            aac_encoder: aac_encoder_available,
            buffered_encoder: aac_encoder,
            after_sender_spawn: || {},
        }
    }
}

/// Can the buffered stream's encoder be made on this PC? A throwaway
/// encoder on this thread; the sender builds its own on its thread.
fn aac_encoder_available() -> Result<()> {
    #[cfg(windows)]
    return crate::airplay::aac_mf::AacEncoder::new().map(drop);
    #[cfg(not(windows))]
    return Err(anyhow::anyhow!("AAC is only available on Windows"));
}

/// The UDP sockets a general-path session binds before it connects, and
/// their ports.
struct Ap2Sockets {
    audio: UdpSocket,
    control: UdpSocket,
    timing: UdpSocket,
    control_port: u16,
    timing_port: u16,
}

impl Drop for AirPlay2Session {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
    }
}

/// The threads of one session.
#[derive(Default)]
struct SessionThreads {
    sender: Option<JoinHandle<()>>,
    timing: Option<JoinHandle<()>>,
    sync: Option<JoinHandle<()>>,
    resend: Option<JoinHandle<()>>,
    event: Option<JoinHandle<()>>,
    feedback: Option<JoinHandle<()>>,
    volume: Option<JoinHandle<()>>,
    /// Per-member threads of a stereo pair beyond those above.
    extra: Vec<JoinHandle<()>>,
}

impl SessionThreads {
    /// Wait for every thread to end; the caller has set the stop flag.
    fn join(&mut self) {
        let fixed = [
            self.sender.take(),
            self.timing.take(),
            self.sync.take(),
            self.resend.take(),
            self.event.take(),
            self.feedback.take(),
            self.volume.take(),
        ];
        for h in fixed.into_iter().flatten().chain(self.extra.drain(..)) {
            let _ = h.join();
        }
    }
}

/// A bring-up in progress: the threads it has started, its PTP
/// registration and its control connections. If a step fails (this is
/// dropped before [`BringUp::finish`]), it ends them in this order: stop
/// flag set and every thread joined, so the threads have closed their
/// connections (the event channel among them) and none can take the
/// receiver's reaction to the teardown for a dropped session; then the PTP
/// registration leaves; then the control connections are let go, and the
/// last reference to each sends TEARDOWN (once paired). Declaration order
/// in the bring-up code doesn't matter: the bring-up holds a reference to
/// every connection until its own drop.
struct BringUp {
    stop_flag: Arc<AtomicBool>,
    faults: FaultSlot,
    threads: SessionThreads,
    ptp: Option<PtpMaster>,
    conns: Vec<Arc<Mutex<Ap2Rtsp>>>,
    finished: bool,
}

impl BringUp {
    fn new(stop_flag: &Arc<AtomicBool>, faults: &FaultSlot) -> Self {
        Self {
            stop_flag: stop_flag.clone(),
            faults: faults.clone(),
            threads: SessionThreads::default(),
            ptp: None,
            conns: Vec::new(),
            finished: false,
        }
    }

    /// Keep `conn` until the bring-up has ended its threads.
    fn hold(&mut self, conn: &Arc<Mutex<Ap2Rtsp>>) {
        self.conns.push(conn.clone());
    }

    /// The bring-up succeeded: the threads and the PTP registration now
    /// belong to the session, and its faults count for the repeat rule.
    fn finish(mut self) -> (SessionThreads, Option<PtpMaster>) {
        self.finished = true;
        self.faults.arm(Instant::now());
        (std::mem::take(&mut self.threads), self.ptp.take())
    }
}

impl Drop for BringUp {
    fn drop(&mut self) {
        if !self.finished {
            self.stop_flag.store(true, Ordering::Release);
            self.threads.join();
            drop(self.ptp.take());
        }
        self.conns.clear();
    }
}

/// The realtime codec every member accepts: PCM when all allow it, else
/// ALAC when all allow that.
fn common_codec(members: &[AirPlayRenderer]) -> Option<RealtimeCodec> {
    let picks: Vec<Option<RealtimeCodec>> = members.iter().map(|m| RealtimeCodec::from_cn(&m.codecs)).collect();
    if picks.iter().all(|p| *p == Some(RealtimeCodec::Pcm)) {
        Some(RealtimeCodec::Pcm)
    } else if members.iter().all(|m| m.codecs.is_empty() || m.codecs.contains(&1)) {
        Some(RealtimeCodec::Alac)
    } else {
        None
    }
}

/// Pair one receiver on its RTSP connection: `pair-verify` with its stored
/// pairing when there is one, transient pairing otherwise. Returns the
/// 32-byte audio key.
fn pair_member(
    rtsp: &mut Ap2Rtsp,
    stored: Option<&StoredPairing>,
    name: &str,
) -> std::result::Result<[u8; 32], Ap2StartError> {
    // A PIN-paired receiver (Apple TV with access control) gets
    // pair-verify from the stored keys and nothing else: falling back to
    // transient pairing would let anyone who can fake a refusal get an
    // unauthenticated session. Every failure leaves the stored pairing as
    // it is. Everything else gets transient pairing; a 470 there means the
    // receiver needs the one-time PIN ceremony.
    let key = if let Some(stored) = stored {
        info!("AirPlay 2: verifying stored pairing with {}", name);
        match rtsp.pair_verify(stored) {
            Ok(key) => key,
            Err(PairVerifyError::NoAnswer(e)) => {
                return Err(Ap2StartError::Other(e.context("AirPlay 2 HomeKit pair-verify: no usable answer")));
            }
            Err(PairVerifyError::NotProven(e)) => {
                warn!(
                    "AirPlay 2: {} answered pair-verify without proving it is the paired device ({:#}); \
                     the stored pairing is kept",
                    name, e
                );
                return Err(Ap2StartError::PairingNotProven(e));
            }
            Err(PairVerifyError::Refused(e)) => {
                warn!(
                    "AirPlay 2: {} refused this PC's stored pairing ({:#}); it is kept until a new PIN \
                     pairing replaces it or the user forgets it",
                    name, e
                );
                return Err(Ap2StartError::PairingRefused(e));
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
                    name
                );
                return Err(Ap2StartError::NeedsPin);
            }
        }
    };
    Ok(key)
}

/// `/feedback` keepalive every [`FEEDBACK_INTERVAL`]; policy lives in
/// [`FeedbackMonitor`].
///
/// HomePods (`strict`): one request in flight at a time, polled without
/// holding the shared connection while the receiver is slow — only ever
/// taken with `try_lock` for a few ms, so a volume change (whose caller
/// may hold the app's session lock) never waits behind it. Other
/// receivers: a plain synchronous request per interval.
fn spawn_feedback_keepalive(
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    stop_flag: Arc<AtomicBool>,
    faults: FaultSlot,
    strict: bool,
    receiver_name: String,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-feedback:{}", receiver_name))
        .spawn(move || {
            let mut monitor = FeedbackMonitor::new(strict);
            if !strict {
                // Other receivers: one synchronous request every interval,
                // as always; three failures in a row drop the session.
                while crate::airplay::timing::sleep_unless_stopped(&stop_flag, FEEDBACK_INTERVAL) {
                    let result = rtsp.lock().unwrap().feedback_status();
                    let action = match result {
                        Ok(status) => monitor.on_status(status, Instant::now()),
                        Err(e) => monitor.on_error(format!("{e:#}")),
                    };
                    if monitor.failures == 1 && action == FeedbackAction::None {
                        warn!("AirPlay 2 /feedback failed (continuing)");
                    }
                    if let FeedbackAction::Fault(f) = action {
                        if !stop_flag.load(Ordering::Acquire) {
                            faults.raise(f);
                        }
                        break;
                    }
                }
                debug!("AirPlay 2 feedback keepalive exiting ({} failures)", monitor.failures);
                return;
            }
            let mut cseq = 0u32;
            let mut next_due = Instant::now() + FEEDBACK_INTERVAL;
            while !stop_flag.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(100));
                let now = Instant::now();
                if !monitor.outstanding() && now < next_due {
                    continue;
                }
                let Ok(mut guard) = rtsp.try_lock() else { continue };
                let polled = if monitor.outstanding() {
                    guard.poll_status(cseq, Duration::from_millis(20))
                } else {
                    next_due = now + FEEDBACK_INTERVAL;
                    match guard.send_feedback() {
                        Ok(c) => {
                            cseq = c;
                            monitor.on_sent(now);
                            // Most replies take a few ms; catch them now.
                            guard.poll_status(cseq, Duration::from_millis(50))
                        }
                        Err(e) => Err(e),
                    }
                };
                drop(guard);
                let action = match polled {
                    Ok(Some(status)) => monitor.on_status(status, Instant::now()),
                    Ok(None) => monitor.on_tick(Instant::now()),
                    Err(e) => monitor.on_error(format!("control connection: {e:#}")),
                };
                match action {
                    FeedbackAction::None => {}
                    FeedbackAction::Delayed => log_notice(&receiver_name, "feedback delayed", false),
                    FeedbackAction::Recovered => log_notice(&receiver_name, "feedback", true),
                    FeedbackAction::Fault(f) => {
                        if !stop_flag.load(Ordering::Acquire) {
                            faults.raise(f);
                        }
                        break;
                    }
                }
            }
            debug!(
                "AirPlay 2 feedback keepalive exiting ({} failures, last rtt {:?})",
                monitor.failures, monitor.last_rtt
            );
        })
        .ok()
}

/// Read the receiver's volume back (`GET /info` → `initialVolume`) every
/// second. The request is sent and its reply polled in short `try_lock`
/// slices, so the connection is never held for long and a busy one just
/// skips a round. Stops for good if the receiver doesn't report a volume.
fn spawn_volume_reader(
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    volume: Arc<Mutex<VolumeTracker>>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-volume:{}", receiver_name))
        .spawn(move || {
            let mut last_logged: Option<DeviceVolume> = None;
            while crate::airplay::timing::sleep_unless_stopped(&stop_flag, VOLUME_POLL_INTERVAL) {
                let Some(read) = read_volume_once(&rtsp, &stop_flag) else { continue };
                let mut tracker = volume.lock().unwrap();
                let keep_polling = match read {
                    Ok(InfoVolume::Db(db)) => {
                        tracker.on_read(Some(raop_db_to_volume_pct(db)));
                        true
                    }
                    Ok(InfoVolume::Missing | InfoVolume::Refused(_)) => {
                        tracker.on_read(None);
                        false
                    }
                    // Transport trouble is the keepalive's call; keep trying.
                    Err(e) => {
                        debug!("AirPlay 2 {}: volume read-back failed: {:#}", receiver_name, e);
                        tracker.on_read(None);
                        true
                    }
                };
                let state = tracker.state(Instant::now());
                drop(tracker);
                if last_logged != Some(state) {
                    debug!("AirPlay 2 {}: device volume {:?}", receiver_name, state);
                    last_logged = Some(state);
                }
                if !keep_polling {
                    info!(
                        "AirPlay 2 {}: receiver does not report its volume; read-back stopped",
                        receiver_name
                    );
                    break;
                }
            }
        })
        .ok()
}

/// One read-back round. `None` = skipped (connection busy, reply lost or
/// slower than [`VOLUME_POLL_TIMEOUT`], or stopping).
fn read_volume_once(rtsp: &Arc<Mutex<Ap2Rtsp>>, stop_flag: &AtomicBool) -> Option<Result<InfoVolume>> {
    let cseq = match rtsp.try_lock().ok()?.send_info() {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    let deadline = Instant::now() + VOLUME_POLL_TIMEOUT;
    while Instant::now() < deadline && !stop_flag.load(Ordering::Acquire) {
        if let Ok(mut guard) = rtsp.try_lock() {
            match guard.poll_info_volume(cseq, Duration::from_millis(20)) {
                Ok(InfoPoll::Done(v)) => return Some(Ok(v)),
                Ok(InfoPoll::Lost) => return None,
                Ok(InfoPoll::Pending) => {}
                Err(e) => return Some(Err(e)),
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

// ---------------------------------------------------------------------------
// Buffered audio sender (type 103 — length-prefixed sealed packets on TCP)
// ---------------------------------------------------------------------------

/// Samples per buffered packet: one AAC-LC frame (the SETUP's `spf`).
const BUFFERED_SPF: usize = 1024;

/// The SSRC Apple's senders put on buffered AAC-LC 44.1 kHz stereo
/// packets. Receivers read a buffered packet's SSRC as its payload's
/// format: shairport-sync plays no block whose SSRC it doesn't know.
const BUFFERED_AAC_SSRC: u32 = 0x1600_0000;

/// What makes the buffered stream's payloads, on the sender thread: the
/// AAC encoder must live on the thread that uses it (COM).
fn aac_encoder() -> EncoderFactory {
    #[cfg(windows)]
    return Box::new(|| Ok(Box::new(crate::airplay::aac_mf::AacEncoder::new()?) as Box<dyn BufferedEncoder>));
    #[cfg(not(windows))]
    return Box::new(|| Err(anyhow::anyhow!("AAC is only available on Windows")));
}

/// Turns one packet's worth of PCM ([`BUFFERED_SPF`] stereo frames,
/// interleaved) into zero or more payloads: the AAC MFT buffers a frame or
/// two before its first output.
trait BufferedEncoder {
    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>>;
}

/// Makes the buffered sender's encoder; called once, on the sender thread.
type EncoderFactory = Box<dyn FnOnce() -> Result<Box<dyn BufferedEncoder>> + Send>;

#[cfg(windows)]
impl BufferedEncoder for crate::airplay::aac_mf::AacEncoder {
    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>> {
        crate::airplay::aac_mf::AacEncoder::encode(self, pcm)
    }
}

struct BufferedSenderConfig {
    stream: TcpStream,
    audio_key: [u8; 32],
    initial_seq: u16,
    initial_rtptime: u32,
    samples_rx: Receiver<PcmFrame>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
    current_rtptime: Arc<AtomicU32>,
    /// The next packet's 23-bit sequence number — FLUSHBUFFERED's
    /// `flushUntilSeq` at stop().
    last_seq: Arc<AtomicU32>,
    /// Shared RTSP connection — the sender anchors on it at first audio.
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    /// Both PTP timelines (ours + the receiver's, once locked).
    timeline: PtpTimeline,
    /// Makes the AAC encoder, on the sender thread.
    encoder: EncoderFactory,
    /// How long a refused or unanswered anchor is retried.
    anchor_retry: AnchorRetry,
    /// The receiver is a HomePod (its settings differ from the others').
    homepod: bool,
    faults: FaultSlot,
}

/// Send SETRATEANCHORTIME for the buffered stream: "rtpTime plays at
/// (timeline now + lead)". Prefers the RECEIVER's timeline once the PTP
/// layer has locked onto its Sync/Follow_Up stream (field-tested: Sonos
/// refuses anchors on any other clock); falls back to our own timeline
/// for receivers that follow the sender instead. Anchors are rounded up
/// to a whole second so networkTimeFrac is 0. Returns the timeline the
/// anchor named.
fn try_anchor_buffered(
    rtsp: &Arc<Mutex<Ap2Rtsp>>,
    timeline: &PtpTimeline,
    rtp_time: u32,
) -> std::result::Result<u64, AnchorFailure> {
    // Take the connection first: another request (volume, read-back) may
    // hold it for a while, and an anchor computed before that wait could
    // already be in the past when it is sent.
    let mut conn = rtsp.lock().unwrap();
    let (timeline_id, base_ns) = match timeline.receiver_now_ns() {
        Some((id, now)) => (id, now),
        None => (timeline.clock_id, timeline.our_now_ns()),
    };
    // The receiver's clock came off the network: a reading too large to
    // anchor on is a failed attempt, not a wrapped time.
    let Some(anchor_ns) = base_ns
        .checked_add(ANCHOR_LEAD_NS)
        .and_then(|ns| ns.div_ceil(1_000_000_000).checked_mul(1_000_000_000))
    else {
        return Err(AnchorFailure::BadClock(format!(
            "the receiver's clock reads {base_ns} ns, too far ahead to schedule playback on"
        )));
    };
    match conn.set_rate_anchor_time(1, rtp_time, anchor_ns, timeline_id) {
        Ok(Ok(())) => Ok(timeline_id),
        Ok(Err(refused)) => Err(refused),
        Err(e) => Err(AnchorFailure::NoAnswer(format!("{e:#}"))),
    }
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

/// The buffered stream's encoder failed: no audio can follow, so the
/// session ends. SETUP negotiated AAC, so switching codec within the session
/// isn't possible; a reconnect builds a new encoder. If the reconnect's
/// encoder fails too before it played, the person is told.
fn encoder_failed(faults: &FaultSlot, stop_flag: &AtomicBool, detail: String, played: bool) {
    if stop_flag.load(Ordering::Acquire) {
        return;
    }
    faults.raise_unless_repeated("encoder_failed", FaultChannel::Media, detail, played, || {
        "Windows' AAC encoder failed again after reconnecting. The realtime stream doesn't use \
         it: turn on \"Prefer low-latency realtime\" in Advanced, then select the speaker again."
            .to_string()
    });
}

/// No anchor was accepted within the retry bound: the receiver will not
/// play the stream, so the session ends. A reconnect may help once (a fresh
/// session, a fresh PTP exchange); if it fails the same way, the person is
/// told what the code knows ([`anchor_advice`]).
fn anchor_failed(
    faults: &FaultSlot,
    stop_flag: &AtomicBool,
    clock: ClockEvidence,
    ptp_ports_problem: Option<&PortsProblem>,
    homepod: bool,
    last: &AnchorFailure,
) {
    if stop_flag.load(Ordering::Acquire) {
        return;
    }
    let detail = format!("the speaker accepted no playback anchor (last: {last})");
    // No anchor was accepted, so this session never played.
    faults.raise_unless_repeated("anchor_failed", FaultChannel::Media, detail, false, || {
        anchor_failed_message(last, ptp_ports_problem, clock, homepod)
    });
}

/// What the PTP layer saw of the receiver by the time the anchor gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClockEvidence {
    /// Anything at all arrived on the PTP ports.
    heard: bool,
    /// It announced a clock of its own.
    announced: bool,
    /// Its Sync/Follow_Up pairs arrived: we follow its clock.
    locked: bool,
}

/// Which cause the final anchor sentence names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnchorAdvice {
    /// Another program holds a PTP port.
    OtherProgram,
    /// A PTP port couldn't be opened for another reason.
    PortUnavailable,
    /// The receiver's clock never reached this PC: nothing arrived, or it
    /// announced its clock but its time signals never came.
    Firewall,
    /// It follows this PC's clock and refused every anchor on it.
    FollowsOurClock,
    /// We followed its own clock and it refused anyway.
    RefusedOnItsClock,
    NoAnswer,
    BadClock,
}

fn anchor_advice(last: &AnchorFailure, ports: Option<&PortsProblem>, clock: ClockEvidence) -> AnchorAdvice {
    match last {
        AnchorFailure::NoAnswer(_) => AnchorAdvice::NoAnswer,
        AnchorFailure::BadClock(_) => AnchorAdvice::BadClock,
        AnchorFailure::Refused { .. } => match ports {
            Some(p) if p.in_use => AnchorAdvice::OtherProgram,
            Some(_) => AnchorAdvice::PortUnavailable,
            None if !clock.heard || (clock.announced && !clock.locked) => AnchorAdvice::Firewall,
            None if clock.locked => AnchorAdvice::RefusedOnItsClock,
            None => AnchorAdvice::FollowsOurClock,
        },
    }
}

/// The sentence after "Connection to <speaker> ended:" when the anchor
/// failed again after the reconnect.
fn anchor_failed_message(
    last: &AnchorFailure,
    ports: Option<&PortsProblem>,
    clock: ClockEvidence,
    homepod: bool,
) -> String {
    let ports_text = ports.map(|p| p.to_string()).unwrap_or_default();
    match anchor_advice(last, ports, clock) {
        AnchorAdvice::OtherProgram => format!(
            "it would not start playing. AirPlay 2 needs UDP ports 319 and 320 for the speaker's clock, \
             but {ports_text}. Close that program, then select the speaker again."
        ),
        AnchorAdvice::PortUnavailable => format!(
            "it would not start playing. AirPlay 2 needs UDP ports 319 and 320 for the speaker's clock, \
             but {ports_text}. Select the speaker again."
        ),
        AnchorAdvice::Firewall => format!(
            "it refused to start playing, and its clock never reached this PC. Check that Windows \
             Firewall lets {} receive on the network this PC uses (it needs UDP ports 319 and 320), \
             then select the speaker again.",
            crate::PRODUCT_NAME
        ),
        AnchorAdvice::FollowsOurClock if homepod => "it refused to start playing on this PC's clock. Turn off \
             \"Use the buffered stream for single HomePods\" in Advanced, then select it again."
            .to_string(),
        AnchorAdvice::FollowsOurClock => "it refused to start playing on this PC's clock. Turn on \
             \"Prefer low-latency realtime\" in Advanced, then select the speaker again."
            .to_string(),
        AnchorAdvice::RefusedOnItsClock => {
            "it refused to start playing, although this PC was following its clock. Select it again.".to_string()
        }
        AnchorAdvice::NoAnswer => "it stopped answering just before playback was to start. Check that it is \
             switched on and on the same network as this PC, then select it again."
            .to_string(),
        AnchorAdvice::BadClock => {
            "the clock it reported can't be used to schedule playback. Select it again.".to_string()
        }
    }
}

fn spawn_ap2_buffered_sender(cfg: BufferedSenderConfig) -> Result<JoinHandle<()>> {
    let name = format!("stream-to-speaker-ap2-buffered:{}", cfg.receiver_name);
    Ok(std::thread::Builder::new().name(name).spawn(move || run_ap2_buffered_sender(cfg))?)
}

fn run_ap2_buffered_sender(mut cfg: BufferedSenderConfig) {
    use std::io::Write;
    info!(
        "AirPlay 2 buffered sender → {:?} (seq={}, rtptime={})",
        cfg.stream.peer_addr().ok(),
        cfg.initial_seq,
        cfg.initial_rtptime
    );
    let spf = BUFFERED_SPF;
    let mut sealer = AudioSealer::new(&cfg.audio_key, cfg.initial_seq);
    let mut seq = cfg.initial_seq as u32;
    let ssrc = BUFFERED_AAC_SSRC;
    let mut rtptime = cfg.initial_rtptime;
    let mut packet_count: u64 = 0;
    let mut ring: Vec<i16> = Vec::with_capacity(spf * 2);

    // The AAC encoder must live on this thread (COM); the session already
    // probed availability before negotiating ct=4 in SETUP.
    let make_encoder = cfg.encoder;
    let mut encoder = match make_encoder() {
        Ok(enc) => enc,
        Err(e) => {
            encoder_failed(&cfg.faults, &cfg.stop_flag, format!("AAC-LC encoder could not start: {e:#}"), false);
            return;
        }
    };

    let started = Instant::now();
    let packet_duration =
        Duration::from_nanos((spf as u64 * 1_000_000_000) / WIRE_SAMPLE_RATE as u64);
    let mut idle_warned = false;
    let mut anchored = false;
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
                if !anchored && !idle_warned && started.elapsed() > Duration::from_secs(3) {
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
        if !anchored {
            if ring.len() >= spf * 2
                && last_anchor_try.map_or(true, |t| t.elapsed() >= Duration::from_secs(1))
            {
                last_anchor_try = Some(Instant::now());
                match try_anchor_buffered(&cfg.rtsp, &cfg.timeline, rtptime) {
                    Ok(tid) => {
                        anchored = true;
                        pace_start = Instant::now();
                        packet_count = 0;
                        info!(
                            "AirPlay 2: anchored at first audio — rtpTime {} on timeline {:#018x}{}",
                            rtptime,
                            tid,
                            if tid == cfg.timeline.clock_id { " (our clock)" } else { " (receiver's clock)" }
                        );
                    }
                    Err(failure) => match cfg.anchor_retry.on_failure(&failure, Instant::now()) {
                        AnchorAction::Retry { warn: true } => warn!("AirPlay 2 anchor failed (will retry): {failure}"),
                        AnchorAction::Retry { warn: false } => debug!("AirPlay 2 anchor failed again: {failure}"),
                        AnchorAction::GiveUp => {
                            let clock = ClockEvidence {
                                heard: cfg.timeline.heard_from_receiver(),
                                announced: cfg.timeline.receiver_announced_clock(),
                                locked: cfg.timeline.locked_to_receiver(),
                            };
                            anchor_failed(
                                &cfg.faults,
                                &cfg.stop_flag,
                                clock,
                                cfg.timeline.ports_problem().as_ref(),
                                cfg.homepod,
                                &failure,
                            );
                            return;
                        }
                    },
                }
            }
            if !anchored {
                // Bound the pre-anchor buffer to ~2 s of the freshest audio.
                let max = WIRE_SAMPLE_RATE as usize * 2 * 2;
                if ring.len() > max {
                    let cut = ring.len() - max;
                    ring.drain(..cut);
                }
                continue;
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
            let payloads = match encoder.encode(&pkt_samples) {
                Ok(frames) => frames,
                Err(e) => {
                    encoder_failed(
                        &cfg.faults,
                        &cfg.stop_flag,
                        format!("AAC-LC encoding failed: {e:#}"),
                        packet_count > 0,
                    );
                    return;
                }
            };

            for payload in payloads {
                let header = buffered_rtp_header(seq, rtptime, ssrc);
                let sealed = sealer.seal(&header, &payload);
                let mut packet = Vec::with_capacity(12 + sealed.len());
                packet.extend_from_slice(&header);
                packet.extend_from_slice(&sealed);
                let framed = frame_buffered_packet(&packet);

                // Pace to wall-clock from the anchor: the receiver plays
                // rtptime-at-anchor 0.5-1.5 s from now, so staying at the
                // sample rate keeps its buffer bounded on both sides.
                let deadline = pace_start + packet_duration.saturating_mul((packet_count + 1) as u32);
                let now = Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline - now);
                }

                if let Err(e) = cfg.stream.write_all(&framed) {
                    if !cfg.stop_flag.load(Ordering::Acquire) {
                        cfg.faults.raise(Ap2Fault::new(
                            "media_write_failed",
                            FaultChannel::Media,
                            true,
                            format!("buffered send failed (receiver stopped reading?): {e}"),
                        ));
                    }
                    return;
                }
                if packet_count == 0 {
                    cfg.faults.note_playing();
                    info!("AirPlay 2: buffered AAC-LC audio flowing — first packet ({} bytes framed)", framed.len());
                }

                seq = (seq + 1) & BUFFERED_SEQ_MASK;
                rtptime = rtptime.wrapping_add(spf as u32);
                cfg.current_rtptime.store(rtptime, Ordering::Release);
                cfg.last_seq.store(seq, Ordering::Release);
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
    audio_socket: UdpSocket,
    receiver_addr: SocketAddr,
    audio_key: [u8; 32],
    initial_seq: u16,
    initial_rtptime: u32,
    ssrc: u32,
    samples_rx: Receiver<PcmFrame>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
    current_rtptime: Arc<AtomicU32>,
    recovery: RealtimeRecovery,
    faults: FaultSlot,
}

/// How the realtime sender handles retransmission and send errors.
enum RealtimeRecovery {
    /// Packets go into the ring a separate responder thread serves, and a
    /// failed send ends the session.
    Classic { resend: Arc<ResendBuffer> },
    /// HomePods: resend requests are read (1 ms read timeout) and answered
    /// from the media loop after each slot's new audio, only until the
    /// packet's playout deadline (slot + `latency`); send errors are
    /// tolerated for the grace period.
    Deadline { control_socket: UdpSocket, retransmit: Retransmitter, latency: Duration },
}

fn spawn_ap2_sender(cfg: Ap2SenderConfig) -> Result<JoinHandle<()>> {
    let name = format!("stream-to-speaker-ap2-rtp:{}", cfg.receiver_name);
    Ok(std::thread::Builder::new().name(name).spawn(move || run_ap2_sender(cfg))?)
}

fn run_ap2_sender(mut cfg: Ap2SenderConfig) {
    info!(
        "AirPlay 2 RTP sender → {} (seq={}, rtptime={})",
        cfg.receiver_addr, cfg.initial_seq, cfg.initial_rtptime
    );
    // One nonce per packet actually sent, counting on from the first seq;
    // resends replay the recorded on-wire bytes and never consume one.
    let mut sealer = AudioSealer::new(&cfg.audio_key, cfg.initial_seq);
    let mut send_health = SendHealth::new(Instant::now());
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

    let start = Instant::now();
    let packet_duration =
        Duration::from_nanos((FRAMES_PER_PACKET as u64 * 1_000_000_000) / WIRE_SAMPLE_RATE as u64);

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
        let alac = build_uncompressed_alac_frame(&pkt_samples);
        let header = ap2_rtp_header(seq, rtptime, cfg.ssrc, packet_count == 0);
        let sealed = sealer.seal(&header, &alac);
        let mut packet = Vec::with_capacity(12 + sealed.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&sealed);

        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }

        if let RealtimeRecovery::Classic { resend } = &cfg.recovery {
            if let Err(e) = cfg.audio_socket.send_to(&packet, cfg.receiver_addr) {
                cfg.faults.raise(Ap2Fault::new(
                    "media_send_failed",
                    FaultChannel::Media,
                    true,
                    format!("AirPlay 2 RTP send failed: {e}"),
                ));
                return;
            }
            resend.record(seq, &packet);
        }
        // HomePods: a failed send is media loss, not a dead session — Wi-Fi
        // roams and adapter resets fail a few sends and recover. Only a
        // sustained run without one successful send ends the session.
        let action = match &cfg.recovery {
            RealtimeRecovery::Classic { .. } => SendAction::None,
            RealtimeRecovery::Deadline { .. } => match cfg.audio_socket.send_to(&packet, cfg.receiver_addr) {
                Ok(_) => send_health.on_ok(Instant::now()),
                Err(e) => {
                    debug!("AirPlay 2 RTP send failed: {}", e);
                    send_health.on_err(Instant::now())
                }
            },
        };
        match action {
            SendAction::None => {}
            SendAction::Delayed => log_notice(&cfg.receiver_name, "audio send failing", false),
            SendAction::Recovered => log_notice(&cfg.receiver_name, "audio send", true),
            SendAction::Fault(f) => {
                cfg.faults.raise(f);
                return;
            }
        }
        if let RealtimeRecovery::Deadline { control_socket, retransmit, latency } = &mut cfg.recovery {
            // Recorded either way: the receiver may ask for it once the
            // path is back, and the retransmission must carry these bytes.
            retransmit.history.record(seq, packet.clone().into(), deadline, *latency);
            retransmit.service(control_socket, Instant::now());
        }
        if packet_count == 0 {
            cfg.faults.note_playing();
            info!(
                "AirPlay 2: stream open — first packet ({} bytes) sent to {}",
                packet.len(),
                cfg.receiver_addr
            );
        }

        seq = seq.wrapping_add(1);
        rtptime = rtptime.wrapping_add(FRAMES_PER_PACKET as u32);
        cfg.current_rtptime.store(rtptime, Ordering::Release);
        packet_count += 1;
    }
    info!(
        "AirPlay 2 RTP sender stopped after {} packets ({} silence-filled, {} send errors)",
        packet_count, silence_packets, send_health.errors
    );
}

/// Buffered sequence numbers have 23 bits (see [`buffered_rtp_header`]).
const BUFFERED_SEQ_MASK: u32 = 0x7F_FFFF;

/// Build the 12-byte header of a buffered (type 103) audio packet the way
/// Apple's senders do: V=2, the marker bit set on every packet, and that
/// bit's remaining 7 bits plus the next two bytes holding a 23-bit
/// sequence number — receivers read `bytes 0..4 & 0x7FFFFF`
/// (shairport-sync's buffered reader) — then the RTP timestamp and the
/// format SSRC ([`BUFFERED_AAC_SSRC`]).
fn buffered_rtp_header(seq: u32, timestamp: u32, ssrc: u32) -> [u8; 12] {
    let mut h = [0u8; 12];
    BigEndian::write_u32(&mut h[0..4], 0x8080_0000 | (seq & BUFFERED_SEQ_MASK));
    BigEndian::write_u32(&mut h[4..8], timestamp);
    BigEndian::write_u32(&mut h[8..12], ssrc);
    h
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

    /// With a stored pairing, a failed pair-verify ends the attempt with
    /// the outcome that says what happened — and never moves on to
    /// transient pairing, which would hand whoever answered an
    /// unauthenticated session.
    #[test]
    fn a_failed_pair_verify_never_falls_back_to_transient_pairing() {
        use crate::airplay::test_peers::{self, stored_pairing, Peer, M4};
        for (peer, want) in [
            (Peer::StatusAtM1("400 Bad Request"), "not proven"),
            (Peer::OtherDevice, "not proven"),
            (Peer::Genuine(M4::Error(2)), "refused"),
            (Peer::SilentAtM1, "other"),
        ] {
            let what = format!("{peer:?}");
            let (port, seen) = test_peers::spawn(peer);
            let ip: IpAddr = "127.0.0.1".parse().unwrap();
            let mut rtsp = Ap2Rtsp::connect(ip, port, ip, Duration::from_millis(500)).unwrap();
            let res = pair_member(&mut rtsp, Some(&stored_pairing()), "Apple TV");
            drop(rtsp);
            let got = match res {
                Err(Ap2StartError::PairingNotProven(_)) => "not proven",
                Err(Ap2StartError::PairingRefused(_)) => "refused",
                Err(Ap2StartError::Other(_)) => "other",
                Err(Ap2StartError::NeedsPin) => "needs PIN",
                Ok(_) => "ok",
            };
            assert_eq!(got, want, "{what}");
            let seen = seen.join().unwrap();
            assert!(seen.iter().all(|l| l.starts_with("POST /pair-verify")), "{what}: {seen:?}");
        }
    }

    /// Stands in for the AAC encoder on platforms without one: one payload
    /// per packet, or an error from the `fail_at`-th call on.
    struct ScriptedEncoder {
        calls: usize,
        fail_at: Option<usize>,
    }

    impl BufferedEncoder for ScriptedEncoder {
        fn encode(&mut self, _pcm: &[i16]) -> Result<Vec<Vec<u8>>> {
            self.calls += 1;
            if self.fail_at.is_some_and(|n| self.calls >= n) {
                anyhow::bail!("ProcessInput failed: scripted");
            }
            Ok(vec![vec![0xAA; 16]])
        }
    }

    fn scripted_encoder(fail_at: Option<usize>) -> EncoderFactory {
        Box::new(move || Ok(Box::new(ScriptedEncoder { calls: 0, fail_at }) as Box<dyn BufferedEncoder>))
    }

    /// A buffered sender wired to a scripted receiver on loopback: the
    /// control connection answers SETRATEANCHORTIME with `anchor_status`
    /// (everything else 200); the test reads the data connection.
    struct BufferedRig {
        samples: Option<crossbeam_channel::Sender<PcmFrame>>,
        data: TcpStream,
        stop_flag: Arc<AtomicBool>,
        dead: Arc<AtomicBool>,
        faults: FaultSlot,
        last_seq: Arc<AtomicU32>,
        sender: Option<JoinHandle<()>>,
    }

    impl BufferedRig {
        /// `name`: the receiver's name, which faults are remembered by.
        fn start(name: &str, encoder: EncoderFactory, initial_seq: u16, anchor_replies: &'static [&'static str]) -> Self {
            Self::start_with(name, encoder, initial_seq, anchor_replies, AnchorRetry::new(), None)
        }

        /// `anchor_replies`: the status lines SETRATEANCHORTIME gets, in
        /// order, the last one repeated.
        fn start_with(
            name: &str,
            encoder: EncoderFactory,
            initial_seq: u16,
            anchor_replies: &'static [&'static str],
            anchor_retry: AnchorRetry,
            ptp_ports_problem: Option<crate::airplay::ap2_ptp::PortsProblem>,
        ) -> Self {
            use crate::airplay::test_peers::{read_request, reply};
            use std::io::BufReader;
            let control = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let control_port = control.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let Ok((sock, _)) = control.accept() else { return };
                let mut r = BufReader::new(sock.try_clone().unwrap());
                let mut w = sock;
                let mut anchors = 0;
                while let Some((line, cseq, _)) = read_request(&mut r) {
                    let status = if line.starts_with("SETRATEANCHORTIME") {
                        anchors += 1;
                        anchor_replies[(anchors - 1).min(anchor_replies.len() - 1)]
                    } else {
                        "200 OK"
                    };
                    reply(&mut w, cseq, status, &[]);
                }
            });
            let ip: IpAddr = "127.0.0.1".parse().unwrap();
            let rtsp = Ap2Rtsp::connect(ip, control_port, ip, Duration::from_secs(5)).unwrap();

            let data_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let stream = TcpStream::connect(data_listener.local_addr().unwrap()).unwrap();
            let (data, _) = data_listener.accept().unwrap();
            data.set_read_timeout(Some(Duration::from_secs(10))).unwrap();

            let (samples, samples_rx) = crossbeam_channel::unbounded();
            let stop_flag = Arc::new(AtomicBool::new(false));
            let dead = Arc::new(AtomicBool::new(false));
            let faults = FaultSlot::new(dead.clone(), name.into());
            // As a session that finished its bring-up.
            faults.arm(Instant::now());
            let last_seq = Arc::new(AtomicU32::new(initial_seq as u32));
            let sender = spawn_ap2_buffered_sender(BufferedSenderConfig {
                stream,
                audio_key: [3u8; 32],
                initial_seq,
                initial_rtptime: 1_000,
                samples_rx,
                stop_flag: stop_flag.clone(),
                receiver_name: name.into(),
                current_rtptime: Arc::new(AtomicU32::new(1_000)),
                last_seq: last_seq.clone(),
                rtsp: Arc::new(Mutex::new(rtsp)),
                timeline: PtpTimeline::new(crate::airplay::ap2_ptp::PtpMasterClock::new(), 0x42)
                    .with_ports_problem(ptp_ports_problem),
                encoder,
                anchor_retry,
                homepod: false,
                faults: faults.clone(),
            })
            .unwrap();
            Self { samples: Some(samples), data, stop_flag, dead, faults, last_seq, sender: Some(sender) }
        }

        /// Queue `frames` stereo frames of a quiet tone.
        fn feed(&self, frames: usize) {
            let bytes: Vec<u8> = (0..frames * 2).flat_map(|i| ((i % 64) as i16).to_le_bytes()).collect();
            self.samples.as_ref().unwrap().send(PcmFrame(Arc::new(bytes))).unwrap();
        }

        /// The next packet on the data connection (header and sealed
        /// payload, length prefix removed); None once the sender closed it.
        fn packet(&mut self) -> Option<Vec<u8>> {
            use std::io::Read;
            let mut len = [0u8; 2];
            self.data.read_exact(&mut len).ok()?;
            let mut pkt = vec![0u8; (u16::from_be_bytes(len) as usize).checked_sub(2)?];
            self.data.read_exact(&mut pkt).ok()?;
            Some(pkt)
        }

        /// Stop the sender as the session does and wait for it to exit.
        fn stop(&mut self) {
            self.stop_flag.store(true, Ordering::Release);
            self.samples.take();
            self.sender.take().unwrap().join().unwrap();
        }

        /// Wait for the sender to end on its own (it closes the data
        /// connection as it exits). One that is still running after 10 s is
        /// stopped, so the test reports what it found instead of hanging.
        fn wait_for_exit(&mut self) {
            while self.packet().is_some() {}
            let sender = self.sender.take().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !sender.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !sender.is_finished() {
                self.stop_flag.store(true, Ordering::Release);
                self.samples.take();
            }
            sender.join().unwrap();
        }
    }

    /// The AAC encoder failing ends the session through the fault path:
    /// retryable (the app reconnects), and final, with a sentence for the
    /// person, only when the reconnect fails the same way before it plays.
    /// Failures with playback before them stay retryable.
    #[test]
    fn a_failing_encoder_ends_the_session_and_then_says_why() {
        let failing_start = || -> EncoderFactory { Box::new(|| anyhow::bail!("MFStartup: scripted")) };
        for (round, encoder, retryable) in [
            (1, failing_start(), true),
            (2, failing_start(), false),
            (3, scripted_encoder(Some(3)), true),
            (4, scripted_encoder(Some(3)), true),
        ] {
            let mut rig = BufferedRig::start("Encoder test speaker", encoder, 100, &["200 OK"]);
            rig.feed(BUFFERED_SPF * 8);
            rig.wait_for_exit();
            assert!(rig.dead.load(Ordering::Acquire), "round {round}: session not marked dead");
            let fault = rig.faults.get().unwrap();
            assert_eq!((fault.code, fault.retryable), ("encoder_failed", retryable), "round {round}");
            if !retryable {
                assert!(fault.to_string().contains("Prefer low-latency realtime"), "{fault}");
            }
        }
    }

    /// The final anchor sentence names a cause only where the signal points
    /// at it: another program for an address-in-use error, the firewall when
    /// the receiver's clock never reached this PC (nothing arrived, or it
    /// announced its clock but no time signals came), the realtime setting
    /// for a receiver that follows this PC's clock.
    #[test]
    fn the_anchor_advice_follows_what_the_clock_layer_saw() {
        use crate::airplay::ap2_ptp::PortsProblem;
        let refused = AnchorFailure::Refused { status: 400, detail: "SETRATEANCHORTIME → 400".into() };
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        let in_use = PortsProblem { port: 319, ip, in_use: true, error: "address in use".into() };
        let denied = PortsProblem { port: 319, ip, in_use: false, error: "access denied (os error 10013)".into() };
        let ev = |heard, announced, locked| ClockEvidence { heard, announced, locked };
        let none = ev(false, false, false);
        let follows_us = ev(true, false, false);
        let announced_only = ev(true, true, false);
        let locked = ev(true, true, true);
        use AnchorAdvice::*;
        for (failure, ports, clock, want) in [
            (&refused, Some(&in_use), locked, OtherProgram),
            (&refused, Some(&denied), none, PortUnavailable),
            (&refused, None, none, Firewall),
            (&refused, None, announced_only, Firewall),
            (&refused, None, follows_us, FollowsOurClock),
            (&refused, None, locked, RefusedOnItsClock),
            (&AnchorFailure::NoAnswer("timed out".into()), Some(&in_use), none, NoAnswer),
            (&AnchorFailure::BadClock("too far".into()), None, locked, BadClock),
        ] {
            assert_eq!(anchor_advice(failure, ports, clock), want, "{failure:?} {ports:?} {clock:?}");
        }
        let text = |failure, ports, clock, homepod| anchor_failed_message(failure, ports, clock, homepod);
        assert!(text(&refused, Some(&in_use), locked, false).contains("another program on this PC is using UDP port 319"));
        let neutral = text(&refused, Some(&denied), none, false);
        assert!(neutral.contains("could not open UDP port 319") && !neutral.contains("program"), "{neutral}");
        let firewall = text(&refused, None, none, false);
        assert!(firewall.contains("Windows Firewall") && !firewall.contains("private"), "{firewall}");
        assert!(text(&refused, None, follows_us, false).contains("Prefer low-latency realtime"));
        assert!(text(&refused, None, follows_us, true).contains("Use the buffered stream for single HomePods"));
        assert!(!text(&refused, None, locked, false).contains("Firewall"));
    }

    /// A receiver that refuses every anchor (the Sonos does on our clock
    /// when this PC never gets its clock) ends the session after the bound
    /// instead of retrying for ever in silence; after the reconnect the
    /// same thing is final, with what to check. A receiver that refuses a
    /// few and then accepts plays on.
    #[test]
    fn an_anchor_refused_for_good_ends_the_session_and_then_says_why() {
        let start = |name, retry_for, ports_problem| {
            BufferedRig::start_with(
                name,
                scripted_encoder(None),
                7,
                &["400 Bad Request"],
                AnchorRetry::giving_up_after(retry_for),
                ports_problem,
            )
        };
        let spf = BUFFERED_SPF;
        for (round, ports_problem, want) in [
            (1, None, None),
            (2, None, Some("UDP ports 319 and 320")),
            (1, Some(true), None),
            (2, Some(true), Some("another program on this PC is using UDP port 319")),
        ] {
            let name = if ports_problem.is_some() { "Ports test speaker" } else { "Anchor test speaker" };
            let problem = ports_problem.map(|in_use| crate::airplay::ap2_ptp::PortsProblem {
                port: 319,
                ip: "10.0.0.5".parse().unwrap(),
                in_use,
                error: "address in use".into(),
            });
            let mut rig = start(name, Duration::from_millis(1500), problem);
            rig.feed(spf * 4);
            rig.wait_for_exit();
            assert!(rig.dead.load(Ordering::Acquire), "round {round}: session not marked dead");
            let fault = rig.faults.get().unwrap();
            assert_eq!((fault.code, fault.retryable), ("anchor_failed", round == 1), "round {round}: {fault:?}");
            if let Some(want) = want {
                assert!(fault.to_string().contains(want), "{fault}");
            }
        }

        let mut rig = BufferedRig::start(
            "Late anchor test speaker",
            scripted_encoder(None),
            7,
            &["400 Bad Request", "400 Bad Request", "200 OK"],
        );
        rig.feed(spf * 4);
        assert!(rig.packet().is_some(), "no audio after the anchor was accepted");
        rig.stop();
        assert!(rig.faults.get().is_none());
    }

    /// A session that played clears what was remembered for its speaker,
    /// so "failed again after reconnecting" is only ever said of the
    /// reconnect: fail, play, fail is retryable both times.
    #[test]
    fn a_session_that_played_clears_what_was_remembered() {
        let name = "Played test speaker";
        let fail_at_start = || -> EncoderFactory { Box::new(|| anyhow::bail!("MFStartup: scripted")) };
        let failed = |encoder| {
            let mut rig = BufferedRig::start(name, encoder, 1, &["200 OK"]);
            rig.feed(BUFFERED_SPF * 2);
            rig.wait_for_exit();
            rig.faults.get().expect("a fault")
        };
        assert!(failed(fail_at_start()).retryable, "first failure");
        let mut rig = BufferedRig::start(name, scripted_encoder(None), 1, &["200 OK"]);
        rig.feed(BUFFERED_SPF * 2);
        assert!(rig.packet().is_some(), "the session in between plays");
        rig.stop();
        assert!(rig.faults.get().is_none());
        let again = failed(fail_at_start());
        assert!(again.retryable, "a session played in between: {again:?}");
    }

    /// The audio source going away means the app is replacing or stopping
    /// the session: the sender ends without a fault.
    #[test]
    fn the_source_going_away_ends_the_sender_quietly() {
        let mut rig = BufferedRig::start("Quiet test speaker", scripted_encoder(None), 1, &["200 OK"]);
        rig.feed(BUFFERED_SPF * 2);
        assert!(rig.packet().is_some());
        rig.samples.take();
        rig.wait_for_exit();
        assert!(!rig.dead.load(Ordering::Acquire));
        assert!(rig.faults.get().is_none());
    }

    /// Buffered packets look like an Apple sender's: the marker bit on
    /// every packet with a 23-bit sequence number that runs on past
    /// 0xFFFF, the SSRC naming the format (AAC-LC 44.1 kHz stereo), and
    /// FLUSHBUFFERED's sequence number in the same 23-bit space.
    #[test]
    fn buffered_packets_carry_apples_header_and_format_ssrc() {
        let mut rig = BufferedRig::start("Header test speaker", scripted_encoder(None), 65_530, &["200 OK"]);
        rig.feed(BUFFERED_SPF * 12);
        let mut packets = Vec::new();
        for _ in 0..12 {
            packets.push(rig.packet().expect("a packet"));
        }
        rig.stop();
        while let Some(p) = rig.packet() {
            packets.push(p);
        }
        for (i, p) in packets.iter().enumerate() {
            let what = format!("packet {i}: {:02x?}", &p[..12]);
            assert_eq!(p[0], 0x80, "{what}");
            assert_eq!(p[1] & 0x80, 0x80, "marker bit, {what}");
            let seq = u32::from_be_bytes([p[0], p[1], p[2], p[3]]) & 0x7F_FFFF;
            assert_eq!(seq, 65_530 + i as u32, "{what}");
            assert_eq!(u32::from_be_bytes([p[4], p[5], p[6], p[7]]), 1_000 + (i * BUFFERED_SPF) as u32, "{what}");
            assert_eq!(u32::from_be_bytes([p[8], p[9], p[10], p[11]]), 0x1600_0000, "{what}");
        }
        // FLUSHBUFFERED names the next packet in the same numbering.
        assert_eq!(rig.last_seq.load(Ordering::Acquire), 65_530 + packets.len() as u32);
    }

    /// A bring-up that fails after the event channel is open (here the
    /// receiver refuses the stream SETUP) has stopped that channel's thread
    /// and closed its connection by the time the error is returned,
    /// instead of leaving both behind for every failed attempt.
    #[test]
    fn a_failed_bring_up_closes_what_it_opened_before_returning() {
        use crate::airplay::test_peers::{read_request, reply};
        use std::io::{BufReader, Read};
        let events = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let event_port = events.local_addr().unwrap().port();
        let control = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let control_port = control.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (sock, _) = control.accept().unwrap();
            let mut r = BufReader::new(sock.try_clone().unwrap());
            let mut w = sock;
            let mut lines = Vec::new();
            while let Some((line, cseq, _)) = read_request(&mut r) {
                if line.starts_with("SETUP") && !lines.iter().any(|l: &String| l.starts_with("SETUP")) {
                    let mut d = plist::Dictionary::new();
                    d.insert("eventPort".into(), plist::Value::Integer((event_port as u64).into()));
                    let mut body = Vec::new();
                    plist::to_writer_binary(&mut body, &plist::Value::Dictionary(d)).unwrap();
                    reply(&mut w, cseq, "200 OK", &body);
                } else if line.starts_with("SETUP") {
                    reply(&mut w, cseq, "500 Internal Server Error", &[]);
                } else {
                    reply(&mut w, cseq, "200 OK", &[]);
                }
                lines.push(line);
            }
            lines
        });

        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let renderer = AirPlayRenderer {
            friendly_name: "Bring-up test speaker".into(),
            mac_id: "AA".into(),
            ip,
            port: control_port,
            airplay_port: Some(control_port),
            encryption_types: vec![0],
            codecs: vec![0, 1],
            password_protected: false,
            encryption_key_required: false,
            features: None,
            pk: None,
            model: None,
            group: None,
            pair: None,
            raop: Default::default(),
        };
        let (_samples, samples_rx) = crossbeam_channel::unbounded();
        let cfg = AirPlay2SessionConfig {
            renderer,
            local_ip: ip,
            samples_rx,
            initial_volume: None,
            prefer_realtime: false,
            latency_ms: 2000,
            stored_pairing: None,
            homepod_realtime: true,
            homepod_latency_ms: 200,
        };
        let (audio, control, timing) = (bind_udp(ip).unwrap(), bind_udp(ip).unwrap(), bind_udp(ip).unwrap());
        let sockets = Ap2Sockets {
            control_port: control.local_addr().unwrap().port(),
            timing_port: timing.local_addr().unwrap().port(),
            audio,
            control,
            timing,
        };
        let rtsp = Ap2Rtsp::connect(ip, control_port, ip, Duration::from_secs(5)).unwrap();
        let result = AirPlay2Session::start_paired(cfg, rtsp, [0u8; 32], sockets, BringUpEnv::real());
        assert!(matches!(result, Err(Ap2StartError::Other(_))), "the refused stream SETUP fails the bring-up");

        let (mut event_conn, _) = events.accept().unwrap();
        event_conn.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut b = [0u8; 16];
        assert!(
            matches!(event_conn.read(&mut b), Ok(0)),
            "the event channel was still open after start() returned"
        );
        // The control connection closed with the bring-up too (a paired
        // one sends TEARDOWN first; this test's is unpaired).
        let lines = peer.join().unwrap();
        assert_eq!(lines.len(), 2, "SETUP(timing) and the refused SETUP(stream): {lines:?}");
    }

    /// A bring-up that fails stops and waits for its threads before it lets
    /// go of its control connections. A receiver that answers the closing
    /// control connection (or its TEARDOWN) by resetting the event channel
    /// is therefore not taken for a session that dropped: no fault, and
    /// nothing for the repeat rule.
    #[test]
    fn a_failed_bring_up_ends_its_threads_before_its_connections() {
        use crate::airplay::test_peers::read_request;
        use std::io::BufReader;
        let events = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let event_port = events.local_addr().unwrap().port();
        let control = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let control_port = control.local_addr().unwrap().port();
        let receiver = std::thread::spawn(move || {
            let (ctl, _) = control.accept().unwrap();
            let (event, _) = events.accept().unwrap();
            // Until the sender closes the control connection, then reset
            // the event channel at once.
            let mut r = BufReader::new(ctl);
            while read_request(&mut r).is_some() {}
            socket2::SockRef::from(&event).set_linger(Some(Duration::ZERO)).unwrap();
            drop(event);
        });
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let faults = FaultSlot::new(Arc::new(AtomicBool::new(false)), "Order test HomePod".into());
        {
            let mut bring_up = BringUp::new(&stop_flag, &faults);
            let rtsp = Arc::new(Mutex::new(Ap2Rtsp::connect(ip, control_port, ip, Duration::from_secs(5)).unwrap()));
            bring_up.hold(&rtsp);
            bring_up.threads.event =
                spawn_event_channel(ip, event_port, None, stop_flag.clone(), faults.clone(), true, "Order test HomePod".into());
            assert!(bring_up.threads.event.is_some());
            // A later step fails here: everything above is dropped.
        }
        receiver.join().unwrap();
        assert!(faults.get().is_none(), "the bring-up's own teardown was taken for a drop: {:?}", faults.get());
        assert!(!faults.armed());
    }

    /// A realtime bring-up that succeeds sends its requests in the order it
    /// always has, and stopping the session ends its threads (the event
    /// channel closes) and sends TEARDOWN.
    #[test]
    fn a_session_that_comes_up_stops_everything_it_started() {
        use crate::airplay::test_peers::{read_request, reply};
        use std::io::{BufReader, Read};
        let events = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let event_port = events.local_addr().unwrap().port();
        let media = UdpSocket::bind("127.0.0.1:0").unwrap();
        let media_port = media.local_addr().unwrap().port();
        let control = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let control_port = control.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (sock, _) = control.accept().unwrap();
            let mut r = BufReader::new(sock.try_clone().unwrap());
            let mut w = sock;
            let mut lines = Vec::new();
            let plist_body = |d: plist::Dictionary| {
                let mut body = Vec::new();
                plist::to_writer_binary(&mut body, &plist::Value::Dictionary(d)).unwrap();
                body
            };
            while let Some((line, cseq, _)) = read_request(&mut r) {
                let setups = lines.iter().filter(|l: &&String| l.starts_with("SETUP")).count();
                let body = match (line.starts_with("SETUP"), setups) {
                    (true, 0) => {
                        let mut d = plist::Dictionary::new();
                        d.insert("eventPort".into(), plist::Value::Integer((event_port as u64).into()));
                        plist_body(d)
                    }
                    (true, _) => {
                        let mut s0 = plist::Dictionary::new();
                        s0.insert("dataPort".into(), plist::Value::Integer((media_port as u64).into()));
                        s0.insert("controlPort".into(), plist::Value::Integer((media_port as u64).into()));
                        let mut d = plist::Dictionary::new();
                        d.insert("streams".into(), plist::Value::Array(vec![plist::Value::Dictionary(s0)]));
                        plist_body(d)
                    }
                    _ => Vec::new(),
                };
                reply(&mut w, cseq, "200 OK", &body);
                lines.push(line);
            }
            lines
        });

        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let renderer = AirPlayRenderer {
            friendly_name: "Session test speaker".into(),
            mac_id: "AA".into(),
            ip,
            port: control_port,
            airplay_port: Some(control_port),
            encryption_types: vec![0],
            codecs: vec![0, 1],
            password_protected: false,
            encryption_key_required: false,
            features: None,
            pk: None,
            model: None,
            group: None,
            pair: None,
            raop: Default::default(),
        };
        let (_samples, samples_rx) = crossbeam_channel::unbounded();
        let cfg = AirPlay2SessionConfig {
            renderer,
            local_ip: ip,
            samples_rx,
            initial_volume: None,
            prefer_realtime: false,
            latency_ms: 2000,
            stored_pairing: None,
            homepod_realtime: true,
            homepod_latency_ms: 200,
        };
        let (audio, control, timing) = (bind_udp(ip).unwrap(), bind_udp(ip).unwrap(), bind_udp(ip).unwrap());
        let sockets = Ap2Sockets {
            control_port: control.local_addr().unwrap().port(),
            timing_port: timing.local_addr().unwrap().port(),
            audio,
            control,
            timing,
        };
        let rtsp = Ap2Rtsp::connect(ip, control_port, ip, Duration::from_secs(5)).unwrap();
        let session =
            AirPlay2Session::start_paired(cfg, rtsp, [0u8; 32], sockets, BringUpEnv::real()).unwrap_or_else(|e| panic!("{e:#}"));
        let (mut event_conn, _) = events.accept().unwrap();
        assert!(session.faults.armed(), "a session that came up counts for the repeat rule");
        session.stop();

        event_conn.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut b = [0u8; 16];
        assert!(matches!(event_conn.read(&mut b), Ok(0)), "the event channel was still open after stop()");
        let lines = peer.join().unwrap();
        let methods: Vec<&str> = lines
            .iter()
            .filter(|l| !l.starts_with("POST /feedback"))
            .map(|l| l.split_whitespace().next().unwrap_or(""))
            .collect();
        assert_eq!(methods, ["SETUP", "SETUP", "RECORD", "TEARDOWN"], "{lines:?}");
    }

    /// One general-path session against a scripted receiver that advertises
    /// `features`, brought up and stopped: the request lines and bodies the
    /// receiver got. PTP runs on free ports, never 319/320.
    fn scripted_general_session(
        name: &str,
        features: Option<u64>,
        prefer_realtime: bool,
        aac_encoder: fn() -> Result<()>,
    ) -> Vec<(String, Vec<u8>)> {
        scripted_general_session_with(name, features, prefer_realtime, aac_encoder, aac_encoder_factory_for_tests, || {}, |_| ())
            .0
    }

    /// The real factory on Windows would make a real encoder; tests want a
    /// platform-independent one that works.
    fn aac_encoder_factory_for_tests() -> EncoderFactory {
        scripted_encoder(None)
    }

    /// [`scripted_general_session`] with the buffered sender's encoder
    /// chosen, and `inspect` run on the session before it is stopped.
    fn scripted_general_session_with<T>(
        name: &str,
        features: Option<u64>,
        prefer_realtime: bool,
        aac_encoder: fn() -> Result<()>,
        buffered_encoder: fn() -> EncoderFactory,
        after_sender_spawn: fn(),
        inspect: impl FnOnce(&AirPlay2Session) -> T,
    ) -> (Vec<(String, Vec<u8>)>, T) {
        use crate::airplay::test_peers::{read_request, reply};
        use std::io::BufReader;
        let free_udp = || UdpSocket::bind("127.0.0.1:0").unwrap();
        let (a, b, c, d) = (free_udp(), free_udp(), free_udp(), free_udp());
        let port = |s: &UdpSocket| s.local_addr().unwrap().port();
        let ptp_ports = PtpPorts { local_event: port(&a), local_general: port(&b), peer_event: port(&c), peer_general: port(&d) };
        drop((a, b, c, d));

        let events = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let event_port = events.local_addr().unwrap().port();
        let buffered_data = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let buffered_port = buffered_data.local_addr().unwrap().port();
        let media = UdpSocket::bind("127.0.0.1:0").unwrap();
        let media_port = media.local_addr().unwrap().port();
        let control = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let control_port = control.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (sock, _) = control.accept().unwrap();
            let mut r = BufReader::new(sock.try_clone().unwrap());
            let mut w = sock;
            let mut seen: Vec<(String, Vec<u8>)> = Vec::new();
            let plist_body = |d: plist::Dictionary| {
                let mut body = Vec::new();
                plist::to_writer_binary(&mut body, &plist::Value::Dictionary(d)).unwrap();
                body
            };
            while let Some((line, cseq, body)) = read_request(&mut r) {
                let setups = seen.iter().filter(|(l, _)| l.starts_with("SETUP")).count();
                let answer = match (line.starts_with("SETUP"), setups) {
                    (true, 0) => {
                        let mut d = plist::Dictionary::new();
                        d.insert("eventPort".into(), plist::Value::Integer((event_port as u64).into()));
                        plist_body(d)
                    }
                    (true, _) => {
                        let buffered = stream_setup(&body).get("type").and_then(|v| v.as_unsigned_integer()) == Some(0x67);
                        let data = if buffered { buffered_port } else { media_port };
                        let mut s0 = plist::Dictionary::new();
                        s0.insert("dataPort".into(), plist::Value::Integer((data as u64).into()));
                        s0.insert("controlPort".into(), plist::Value::Integer((media_port as u64).into()));
                        let mut d = plist::Dictionary::new();
                        d.insert("streams".into(), plist::Value::Array(vec![plist::Value::Dictionary(s0)]));
                        plist_body(d)
                    }
                    _ => Vec::new(),
                };
                reply(&mut w, cseq, "200 OK", &answer);
                seen.push((line, body));
            }
            seen
        });

        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let renderer = AirPlayRenderer {
            friendly_name: name.into(),
            // Faults are remembered per receiver id: one per test speaker.
            mac_id: name.into(),
            ip,
            port: control_port,
            airplay_port: Some(control_port),
            encryption_types: vec![0],
            codecs: vec![0, 1],
            password_protected: false,
            encryption_key_required: false,
            features,
            pk: None,
            model: None,
            group: None,
            pair: None,
            raop: Default::default(),
        };
        let (_samples, samples_rx) = crossbeam_channel::unbounded();
        let cfg = AirPlay2SessionConfig {
            renderer,
            local_ip: ip,
            samples_rx,
            initial_volume: None,
            prefer_realtime,
            latency_ms: 2000,
            stored_pairing: None,
            homepod_realtime: true,
            homepod_latency_ms: 200,
        };
        let (audio, control, timing) = (bind_udp(ip).unwrap(), bind_udp(ip).unwrap(), bind_udp(ip).unwrap());
        // The same reported ports in every run, so stream SETUPs compare.
        let sockets = Ap2Sockets { control_port: 50_000, timing_port: 50_001, audio, control, timing };
        let rtsp = Ap2Rtsp::connect(ip, control_port, ip, Duration::from_secs(5)).unwrap();
        let env = BringUpEnv { ptp_ports, aac_encoder, buffered_encoder, after_sender_spawn };
        let session = AirPlay2Session::start_paired(cfg, rtsp, [0u8; 32], sockets, env).unwrap_or_else(|e| panic!("{e:#}"));
        let seen = inspect(&session);
        drop(events);
        session.stop();
        drop((buffered_data, media));
        (peer.join().unwrap(), seen)
    }

    /// `streams[0]` of a stream SETUP body.
    fn stream_setup(body: &[u8]) -> plist::Dictionary {
        let v = plist::Value::from_reader(std::io::Cursor::new(body)).unwrap();
        let streams = v.as_dictionary().unwrap().get("streams").unwrap().as_array().unwrap();
        streams[0].as_dictionary().unwrap().clone()
    }

    /// An encoder that fails as soon as the buffered sender's thread starts,
    /// while the bring-up is still finishing, counts for the repeat rule:
    /// the reconnect that fails the same way before playing is final.
    #[test]
    fn an_encoder_that_fails_as_the_sender_starts_is_remembered() {
        use crate::airplay::discovery::{FEAT_BUFFERED_AUDIO, FEAT_PTP};
        let available: fn() -> Result<()> = || Ok(());
        let failing: fn() -> EncoderFactory = || Box::new(|| anyhow::bail!("CoCreateInstance: scripted"));
        let fault_of = |s: &AirPlay2Session| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !s.is_dead() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            s.fault()
        };
        for round in [1, 2] {
            let (_, fault) = scripted_general_session_with(
                "Early encoder test speaker",
                Some(FEAT_PTP | FEAT_BUFFERED_AUDIO),
                false,
                available,
                failing,
                // The end of the bring-up comes after the sender's failure.
                || std::thread::sleep(Duration::from_millis(200)),
                fault_of,
            );
            let fault = fault.unwrap_or_else(|| panic!("round {round}: no fault"));
            assert_eq!((fault.code, fault.retryable), ("encoder_failed", round == 1), "round {round}");
        }
    }

    /// A receiver that takes the buffered stream gets the realtime one when
    /// Windows' AAC encoder can't be made (a Windows N edition without the
    /// Media Feature Pack): the same requests, and the same stream SETUP a
    /// session sends when the person chose realtime. With the encoder, it
    /// gets buffered AAC as before.
    #[test]
    fn without_the_aac_encoder_a_buffered_receiver_gets_the_realtime_stream() {
        use crate::airplay::discovery::{FEAT_BUFFERED_AUDIO, FEAT_PTP};
        let buffered_receiver = Some(FEAT_PTP | FEAT_BUFFERED_AUDIO);
        let available: fn() -> Result<()> = || Ok(());
        let unavailable: fn() -> Result<()> = || anyhow::bail!("Media Foundation is not installed on this PC");
        let chosen = scripted_general_session("Realtime chosen test speaker", buffered_receiver, true, available);
        let missing = scripted_general_session("No encoder test speaker", buffered_receiver, false, unavailable);
        let with_aac = scripted_general_session("Encoder test speaker 2", buffered_receiver, false, available);

        let methods = |seen: &[(String, Vec<u8>)]| -> Vec<String> {
            seen.iter()
                .filter(|(l, _)| !l.starts_with("POST /feedback"))
                .map(|(l, _)| l.split_whitespace().next().unwrap_or("").to_string())
                .collect()
        };
        assert_eq!(methods(&chosen), ["SETUP", "SETUP", "SETPEERS", "RECORD", "TEARDOWN"]);
        assert_eq!(methods(&missing), methods(&chosen));
        // The stream SETUP, apart from streamConnectionID: the connection's
        // own random RTSP session id, different in any two sessions.
        let stream = |seen: &[(String, Vec<u8>)]| {
            let mut d = stream_setup(&seen[1].1);
            d.remove("streamConnectionID");
            d
        };
        assert_eq!(stream(&missing), stream(&chosen));
        assert_eq!(stream(&chosen).get("type").and_then(|v| v.as_unsigned_integer()), Some(0x60));

        let aac = stream(&with_aac);
        let field = |k: &str| aac.get(k).and_then(|v| v.as_unsigned_integer());
        assert_eq!((field("type"), field("ct"), field("spf"), field("audioFormat")), (Some(0x67), Some(4), Some(1024), Some(0x40_0000)));
    }

    /// Which bring-up a session gets: the HomePod realtime profile for a
    /// stereo pair and for a HomePod on its default setting, as before; and
    /// now also for a HomePod set to the buffered stream when Windows' AAC
    /// encoder can't be made, instead of the general realtime path.
    #[test]
    fn a_homepod_without_the_aac_encoder_gets_the_homepod_profile() {
        use crate::airplay::discovery::{FEAT_BUFFERED_AUDIO, FEAT_PTP};
        let renderer = |model: &str| AirPlayRenderer {
            friendly_name: "Route test".into(),
            mac_id: "AA".into(),
            ip: "10.0.0.2".parse().unwrap(),
            port: 7000,
            airplay_port: Some(7000),
            encryption_types: vec![0],
            codecs: vec![0, 1],
            password_protected: false,
            encryption_key_required: false,
            features: Some(FEAT_PTP | FEAT_BUFFERED_AUDIO),
            pk: None,
            model: Some(model.into()),
            group: None,
            pair: None,
            raop: Default::default(),
        };
        let cfg = |model: &str, homepod_realtime: bool, prefer_realtime: bool| AirPlay2SessionConfig {
            renderer: renderer(model),
            local_ip: "10.0.0.1".parse().unwrap(),
            samples_rx: crossbeam_channel::unbounded().1,
            initial_volume: None,
            prefer_realtime,
            latency_ms: 2000,
            stored_pairing: None,
            homepod_realtime,
            homepod_latency_ms: 200,
        };
        let available: fn() -> Result<()> = || Ok(());
        let missing: fn() -> Result<()> = || anyhow::bail!("Media Foundation could not be loaded");
        use BringUpRoute::*;
        for (cfg, probe, want) in [
            (cfg("AudioAccessory5,1", true, false), missing, HomePodRealtime),
            (cfg("AudioAccessory5,1", false, false), missing, HomePodRealtime),
            (cfg("AudioAccessory5,1", false, false), available, General),
            (cfg("AudioAccessory5,1", false, true), missing, General),
            (cfg("Sonos-Five", false, false), missing, General),
        ] {
            let what = format!("{:?} homepod_realtime={} prefer_realtime={}", cfg.renderer.model, cfg.homepod_realtime, cfg.prefer_realtime);
            assert_eq!(bring_up_route(&cfg, probe), want, "{what}");
        }
    }

    #[test]
    fn buffered_framing_length_includes_itself() {
        let pkt = [0xAAu8; 10];
        let framed = frame_buffered_packet(&pkt);
        assert_eq!(framed.len(), 12);
        // Receiver reads 2-byte BE length, then len-2 more bytes.
        assert_eq!(u16::from_be_bytes([framed[0], framed[1]]), 12);
        assert_eq!(&framed[2..], &pkt);
    }

    #[test]
    fn resend_across_seq_wrap_replays_original_nonce() {
        // Packets recorded across a 16-bit seq wrap are fetched back
        // byte-for-byte: a retransmission carries the nonce the packet was
        // first sealed with and never consumes a fresh one.
        use crate::airplay::ap2_resend::{History, Lookup};
        let mut sealer = AudioSealer::new(&[5u8; 32], 65_530);
        let mut history = History::new(512);
        let slot = Instant::now();
        let mut originals = Vec::new();
        let mut seq: u16 = 65_530;
        for i in 0..12u32 {
            let header = ap2_rtp_header(seq, 0xFFFF_F000u32.wrapping_add(i * 352), 0xABCD, i == 0);
            let mut packet = header.to_vec();
            packet.extend_from_slice(&sealer.seal(&header, &[i as u8; 16]));
            history.record(seq, packet.clone().into(), slot, Duration::from_secs(2));
            originals.push((seq, packet));
            seq = seq.wrapping_add(1);
        }
        let after = sealer.next_counter();
        for (i, (seq, packet)) in originals.iter().enumerate() {
            let Lookup::Hit(got) = history.lookup(*seq, slot) else { panic!("seq {seq} not resendable") };
            assert_eq!(&got[..], &packet[..]);
            assert_eq!(&got[got.len() - 8..], &(65_530 + i as u64).to_le_bytes());
        }
        assert_eq!(sealer.next_counter(), after);
    }

    #[test]
    fn common_codec_needs_every_member() {
        let with_cn = |cn: Vec<u8>| AirPlayRenderer {
            friendly_name: "x".into(),
            mac_id: "AA".into(),
            ip: "10.0.0.2".parse().unwrap(),
            port: 7000,
            airplay_port: Some(7000),
            encryption_types: vec![0],
            codecs: cn,
            password_protected: false,
            encryption_key_required: false,
            features: None,
            pk: None,
            model: Some("AudioAccessory5,1".into()),
            group: None,
            pair: None,
            raop: Default::default(),
        };
        assert_eq!(common_codec(&[with_cn(vec![0, 1, 2, 3]), with_cn(vec![])]), Some(RealtimeCodec::Pcm));
        assert_eq!(common_codec(&[with_cn(vec![0, 1]), with_cn(vec![1])]), Some(RealtimeCodec::Alac));
        assert_eq!(common_codec(&[with_cn(vec![0]), with_cn(vec![1])]), None);
    }

    /// Windows refuses to start a program whose load-time imports are
    /// missing, and N editions have no Media Foundation unless the Media
    /// Feature Pack is installed. A Media Foundation function called through
    /// the `windows` crate is linked as such an import, so the AAC encoder
    /// must find them at run time instead. This guards the source: every
    /// name taken from `windows::Win32::Media::MediaFoundation` must be an
    /// interface, constant or type, never a function.
    #[test]
    fn media_foundation_functions_are_never_linked() {
        fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        /// Interfaces start with `IMF`; constants, GUIDs and structs carry an
        /// underscore or are all capitals. Functions are the rest.
        fn function_shaped(name: &str) -> bool {
            !name.starts_with("IMF") && !name.contains('_') && name.chars().any(|c| c.is_ascii_lowercase())
        }
        let mut files = Vec::new();
        rust_files(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        assert!(files.len() > 20, "found only {} source files", files.len());
        let mut bad = Vec::new();
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            let code: String = text.lines().map(|l| l.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n");
            let mut rest = code.as_str();
            while let Some(at) = rest.find("MediaFoundation") {
                rest = &rest[at + "MediaFoundation".len()..];
                let after = rest.trim_start();
                let Some(path) = after.strip_prefix("::") else {
                    // `MediaFoundation as m` or a bare module import would
                    // hide the names used through it.
                    if after.starts_with("as ") || after.starts_with(';') || after.starts_with(',') {
                        bad.push(format!("{}: imports the module itself", file.display()));
                    }
                    continue;
                };
                let path = path.trim_start();
                let names: Vec<&str> = if let Some(group) = path.strip_prefix('{') {
                    let end = group.find('}').unwrap_or(group.len());
                    group[..end].split(',').map(|n| n.split_whitespace().next().unwrap_or("")).collect()
                } else {
                    vec![path.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '*')).next().unwrap_or("")]
                };
                for name in names.into_iter().filter(|n| !n.is_empty()) {
                    if name == "*" || name == "self" || function_shaped(name) {
                        bad.push(format!("{}: {name}", file.display()));
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "Media Foundation functions linked at load time: look them up at run time, as aac_mf.rs \
             does (if one of these is a type, not a function, teach `function_shaped` about it): {bad:#?}"
        );
    }

    #[test]
    fn buffered_header_holds_a_23_bit_sequence_number() {
        assert_eq!(
            buffered_rtp_header(0x12_3456, 0xAABB_CCDD, 0x1600_0000),
            [0x80, 0x92, 0x34, 0x56, 0xAA, 0xBB, 0xCC, 0xDD, 0x16, 0x00, 0x00, 0x00]
        );
        assert_eq!(buffered_rtp_header(0x7F_FFFF, 0, 0)[..4], [0x80, 0xFF, 0xFF, 0xFF]);
        assert_eq!(buffered_rtp_header(0, 0, 0)[..4], [0x80, 0x80, 0x00, 0x00]);
        assert_eq!((0x7F_FFFFu32 + 1) & BUFFERED_SEQ_MASK, 0);
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
