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
    log_notice, raop_db_to_volume_pct, Ap2Fault, DeviceVolume, FaultChannel, FaultSlot, FeedbackAction,
    FeedbackMonitor, SendAction, SendHealth, VolumeTracker, FEEDBACK_INTERVAL,
};
use crate::airplay::ap2_ptp::{spawn_ptp_master, PtpMaster, PtpTimeline};
use crate::airplay::ap2_resend::Retransmitter;
use crate::airplay::ap2_crypto::ChannelCipher;
use crate::airplay::ap2_realtime::{MemberLink, RealtimeGroup, RealtimeRun};
use crate::airplay::ap2_rtsp::{Ap2Rtsp, InfoPoll, InfoVolume, RealtimeCodec, TransientOutcome};
use crate::airplay::discovery::AirPlayRenderer;
use crate::airplay::hap_pairing::PairingCredentials;
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
    /// Stored HomeKit persistent-pairing credentials for this receiver, if
    /// it was PIN-paired earlier (Apple TV with access control). When
    /// present, the session does `pair-verify` with these instead of
    /// transient pairing.
    pub pairing_creds: Option<PairingCredentials>,
    /// Stream HomePods with the low-latency realtime profile (see
    /// [`AirPlay2Session::start`]); `false` keeps single HomePods on the
    /// general path (buffered when supported). Stereo pairs always use it.
    pub homepod_realtime: bool,
    /// Playout delay for the HomePod realtime profile, in ms.
    pub homepod_latency_ms: u32,
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
    #[error("receiver requires one-time PIN pairing")]
    NeedsPin,
    /// The receiver *answered* pair-verify and rejected the stored
    /// credentials (it forgot the pairing — e.g. the user removed it on
    /// the Apple TV). The caller clears them and re-pairs; OwnTone's
    /// "key cleared + re-prompt on verify failure". Transport failures
    /// during pair-verify deliberately do NOT take this variant — they
    /// surface as [`Ap2StartError::Other`] and leave the credentials
    /// intact.
    #[error("stored pairing no longer accepted by the receiver: {0:#}")]
    VerifyRejected(#[source] anyhow::Error),
    /// Everything else (connect failures, SETUP refusals, timeouts, …).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A live AirPlay 2 session.
pub struct AirPlay2Session {
    pub renderer: AirPlayRenderer,
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    /// Last volume (0..=100) pushed to the receiver; restored on unmute.
    volume_pct: AtomicU32,
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
    sender_handle: Option<JoinHandle<()>>,
    timing_handle: Option<JoinHandle<()>>,
    sync_handle: Option<JoinHandle<()>>,
    resend_handle: Option<JoinHandle<()>>,
    event_handle: Option<JoinHandle<()>>,
    feedback_handle: Option<JoinHandle<()>>,
    volume_handle: Option<JoinHandle<()>>,
    ptp_session: Option<PtpMaster>,
    /// (last sent seq, current rtptime) for the buffered stream — read at
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
    /// Per-member threads beyond the fixed handles above.
    extra_handles: Vec<JoinHandle<()>>,
}

impl AirPlay2Session {
    /// Bring up a session. HomePods with PTP (and every stereo pair) take
    /// the realtime HomePod profile ([`AirPlay2Session::start_homepod_realtime`])
    /// unless `homepod_realtime` is off; every other receiver takes the
    /// general path below.
    pub fn start(cfg: AirPlay2SessionConfig) -> std::result::Result<Self, Ap2StartError> {
        if cfg.renderer.pair.is_some()
            || (cfg.homepod_realtime && cfg.renderer.is_homepod() && cfg.renderer.expects_ptp())
        {
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

        let audio_key = pair_member(&mut rtsp, cfg.pairing_creds.clone(), &cfg.renderer.friendly_name)?;
        info!("AirPlay 2: paired with {}", cfg.renderer.friendly_name);

        let event_ciphers = rtsp.take_event_ciphers();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let faults = FaultSlot::new(dead.clone(), cfg.renderer.friendly_name.clone());
        let resend_stats = Arc::new(crate::airplay::timing::ResendStats::default());

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
        let (ptp_session, timing_setup) = if use_ptp {
            let ptp = spawn_ptp_master(&[cfg.renderer.ip], cfg.local_ip, cfg.renderer.friendly_name.clone(), homepod)
                .context("starting AP2 PTP master")?;
            let ts = rtsp
                .setup_timing_ptp(ptp.timeline.clock_id, &ptp.clock_uuid)
                .context("AP2 SETUP(timing/PTP)")?;
            (Some(ptp), ts)
        } else {
            let ts = rtsp
                .setup_timing_ntp(timing_port)
                .context("AP2 SETUP(timing/NTP)")?;
            (None, ts)
        };
        let event_port = timing_setup.event_port;

        // Open the event channel: the receiver withholds its RECORD response
        // until the sender has a TCP connection to its eventPort. On a
        // HomePod its requests are decrypted and answered 200, and the
        // receiver closing it ends the session; other receivers keep the
        // channel open and drained, as before.
        let event_handle = spawn_event_channel(
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
        // (type 96, UDP) otherwise. Codec: AAC-LC via the Windows-provided
        // Media Foundation encoder — iOS's buffered codec, the only one
        // field-proven on Sonos — with ALAC as fallback (no encoder needed)
        // and realtime as the last resort. Every rejection is visible.
        let latency_samples = crate::airplay::timing::latency_ms_to_samples(cfg.latency_ms);
        let low_latency = cfg.latency_ms < LOW_LATENCY_REALTIME_MS;
        let want_buffered = use_ptp
            && cfg.renderer.supports_buffered_audio()
            && !cfg.prefer_realtime
            && !low_latency;
        if cfg.prefer_realtime {
            info!("AirPlay 2: prefer_realtime_airplay set — using the realtime stream");
        } else if low_latency && use_ptp && cfg.renderer.supports_buffered_audio() {
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
        let (ports, buffered) = if want_buffered {
            let attempt = |rtsp: &mut Ap2Rtsp, k: BufferedCodecKind| match k {
                BufferedCodecKind::Aac => {
                    rtsp.setup_stream_buffered(&audio_key, control_port, 4, 1024, 0x400000, DEFAULT_LATENCY_SAMPLES)
                }
                BufferedCodecKind::Alac => {
                    rtsp.setup_stream_buffered(&audio_key, control_port, 2, 352, 0x40000, DEFAULT_LATENCY_SAMPLES)
                }
            };
            match attempt(&mut rtsp, codec) {
                Ok(p) => {
                    info!(
                        "AirPlay 2: buffered stream accepted (type 103/{}, TCP data port {})",
                        codec.label(),
                        p.data
                    );
                    (p, true)
                }
                Err(e) if codec == BufferedCodecKind::Aac => {
                    warn!("AirPlay 2: buffered AAC SETUP rejected ({e:#}); trying buffered ALAC");
                    codec = BufferedCodecKind::Alac;
                    match attempt(&mut rtsp, codec) {
                        Ok(p) => {
                            info!(
                                "AirPlay 2: buffered stream accepted (type 103/ALAC, TCP data port {})",
                                p.data
                            );
                            (p, true)
                        }
                        Err(e) => {
                            warn!("AirPlay 2: buffered ALAC SETUP rejected ({e:#}); falling back to realtime");
                            let p = rtsp
                                .setup_stream(&audio_key, control_port, latency_samples)
                                .context("AP2 SETUP(stream, realtime fallback)")?;
                            (p, false)
                        }
                    }
                }
                Err(e) => {
                    warn!("AirPlay 2: buffered SETUP rejected ({e:#}); falling back to realtime");
                    let p = rtsp
                        .setup_stream(&audio_key, control_port, latency_samples)
                        .context("AP2 SETUP(stream, realtime fallback)")?;
                    (p, false)
                }
            }
        } else {
            let p = rtsp
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
            if let Err(e) = rtsp.set_peers(&[cfg.renderer.ip, cfg.local_ip]) {
                warn!("AirPlay 2 SETPEERS failed (PTP may not lock): {}", e);
            }
        }

        rtsp.record().context("AP2 RECORD")?;

        let volume = Arc::new(Mutex::new(VolumeTracker::default()));
        if let Some(vol) = cfg.initial_volume {
            let seq = volume.lock().unwrap().on_write(vol, Instant::now());
            if let Err(e) = rtsp.set_volume(volume_pct_to_raop_db(vol)) {
                warn!("AirPlay 2 initial volume failed: {}", e);
                volume.lock().unwrap().on_write_failed(seq);
            }
        }

        // From here the RTSP connection is shared: the buffered sender
        // anchors on it at first audio, the feedback keepalive posts to it,
        // and volume changes arrive from arbitrary threads.
        let rtsp = Arc::new(Mutex::new(rtsp));

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
        let (sender_handle, timing_handle, sync_handle, resend_handle) = if buffered {
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
            let sender = spawn_ap2_buffered_sender(BufferedSenderConfig {
                stream,
                audio_key,
                initial_seq,
                initial_rtptime,
                ssrc,
                samples_rx: cfg.samples_rx,
                stop_flag: stop_flag.clone(),
                receiver_name: cfg.renderer.friendly_name.clone(),
                current_rtptime: current_rtptime.clone(),
                last_seq,
                rtsp: rtsp.clone(),
                timeline: ptp_session.as_ref().unwrap().timeline.clone(),
                codec,
                faults: faults.clone(),
            })?;
            info!(
                "AirPlay 2: buffered {} stream armed — will anchor at first audio",
                codec.label()
            );
            drop(timing_socket);
            drop(control_socket);
            drop(control_for_resend);
            (sender, None, None, None)
        } else {
            // Anchor before audio: both sync spawners send their initial
            // (extension-bit) sync synchronously before returning, so
            // spawning them BEFORE the audio sender guarantees the
            // receiver has a timeline anchor before the first audio
            // packet arrives — same ordering rule as the RAOP path.
            let sync_addr = SocketAddr::new(cfg.renderer.ip, ports.control);
            let (timing_handle, sync_handle) = if use_ptp {
                let ptp = ptp_session.as_ref().unwrap();
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
                // The NTP timing responder is unused under PTP; release its socket.
                drop(timing_socket);
                (None, Some(sync))
            } else {
                let timing = spawn_timing_responder(
                    timing_socket,
                    stop_flag.clone(),
                    cfg.renderer.friendly_name.clone(),
                )
                .context("spawning AP2 timing responder")?;
                let sync = spawn_sync_sender(
                    control_socket,
                    sync_addr,
                    current_rtptime.clone(),
                    latency_samples,
                    stop_flag.clone(),
                    cfg.renderer.friendly_name.clone(),
                )
                .context("spawning AP2 sync sender")?;
                (Some(timing), Some(sync))
            };

            // HomePods: resends from the media loop with per-slot budgets
            // and playout deadlines, and a grace period for send errors.
            // The resend socket gets a 1 ms *read* timeout rather than
            // non-blocking mode, which would also apply to the sync
            // thread's sends on the shared socket. Other receivers keep
            // the resend thread and stop on the first send error.
            let (recovery, resend_handle) = if homepod {
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
                (recovery, None)
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
                (RealtimeRecovery::Classic { resend }, Some(handle))
            };
            let sender = spawn_ap2_sender(Ap2SenderConfig {
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
            })?;
            (sender, timing_handle, sync_handle, resend_handle)
        };

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
        let feedback_handle = spawn_feedback_keepalive(
            rtsp.clone(),
            stop_flag.clone(),
            faults.clone(),
            homepod,
            cfg.renderer.friendly_name.clone(),
        );
        // Volume read-back is limited to HomePods.
        let volume_handle = if homepod {
            spawn_volume_reader(rtsp.clone(), volume.clone(), stop_flag.clone(), cfg.renderer.friendly_name.clone())
        } else {
            None
        };

        Ok(Self {
            renderer: cfg.renderer,
            rtsp,
            volume_pct: AtomicU32::new(cfg.initial_volume.unwrap_or(100)),
            stop_flag,
            dead,
            faults,
            volume,
            resend_stats,
            sender_handle: Some(sender_handle),
            timing_handle,
            sync_handle,
            resend_handle,
            event_handle,
            feedback_handle,
            volume_handle,
            ptp_session,
            buffered_flush,
            data_stream,
            _audio_socket: Some(audio_socket),
            followers: Vec::new(),
            extra_handles: Vec::new(),
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
        let mut stop_guard = StopOnDrop(Some(stop_flag.clone()));
        let dead = Arc::new(AtomicBool::new(false));
        let faults = FaultSlot::new(dead.clone(), label.clone());
        let resend_stats = Arc::new(crate::airplay::timing::ResendStats::default());

        struct Pending {
            renderer: AirPlayRenderer,
            rtsp: Ap2Rtsp,
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
                pair_member(&mut rtsp, cfg.pairing_creds.clone(), &r.friendly_name)?
            } else {
                // A PIN ceremony pairs one receiver under the selected
                // entry's id, so it can't serve a pair: report it plainly
                // instead of prompting for a PIN that would never be used.
                match pair_member(&mut rtsp, None, &r.friendly_name) {
                    Ok(k) => k,
                    Err(Ap2StartError::NeedsPin | Ap2StartError::VerifyRejected(_)) => {
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
        let mut ptp_guard = PtpGuard(Some(
            spawn_ptp_master(&member_ips, cfg.local_ip, label.clone(), true).context("starting AP2 PTP master")?,
        ));
        let ptp = ptp_guard.0.as_ref().expect("just set");

        // 3. Session SETUP + event channel.
        let mut extra_handles = Vec::new();
        for p in &mut pending {
            let ts = p
                .rtsp
                .setup_timing_ptp_homepod(ptp.timeline.clock_id, &ptp.clock_uuid, crate::PRODUCT_NAME)
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
                extra_handles.push(h);
            }
        }

        // 4. Every member gets the same peer list: all members, then us.
        let mut peers = member_ips.clone();
        peers.push(cfg.local_ip);
        for p in &mut pending {
            if let Err(e) = p.rtsp.set_peers(&peers) {
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
                .record_and_flush_at(l.seq, initial_rtptime)
                .with_context(|| format!("AP2 RECORD to {}", p.renderer.friendly_name))?;
        }

        // Initial volume to every member, leader first.
        let volume = Arc::new(Mutex::new(VolumeTracker::default()));
        if let Some(vol) = cfg.initial_volume {
            let seq = volume.lock().unwrap().on_write(vol, Instant::now());
            for p in &mut pending {
                if let Err(e) = p.rtsp.set_volume(volume_pct_to_raop_db(vol)) {
                    warn!("AirPlay 2 initial volume to {} failed: {}", p.renderer.friendly_name, e);
                    volume.lock().unwrap().on_write_failed(seq);
                }
            }
        }

        // 7. Media, keepalives, read-back.
        let group = RealtimeGroup::new(links, initial_rtptime, ptp.timeline.clock.clone(), ptp.timeline.clock_id);
        let sender = crate::airplay::ap2_realtime::spawn(RealtimeRun {
            group,
            codec,
            samples_rx: cfg.samples_rx,
            stop_flag: stop_flag.clone(),
            faults: faults.clone(),
            name: label.clone(),
        })
        .context("spawning AP2 realtime sender")?;

        let mut conns = pending
            .into_iter()
            .map(|p| (p.renderer.friendly_name, Arc::new(Mutex::new(p.rtsp))));
        let (leader_name, rtsp) = conns.next().expect("at least one member");
        let followers: Vec<(String, Arc<Mutex<Ap2Rtsp>>)> = conns.collect();
        let feedback_handle = spawn_feedback_keepalive(rtsp.clone(), stop_flag.clone(), faults.clone(), true, leader_name.clone());
        for (name, conn) in &followers {
            extra_handles.extend(spawn_feedback_keepalive(conn.clone(), stop_flag.clone(), faults.clone(), true, name.clone()));
        }
        let volume_handle = spawn_volume_reader(rtsp.clone(), volume.clone(), stop_flag.clone(), leader_name);

        info!("AirPlay 2: HomePod realtime session up — {}", label);
        stop_guard.0 = None;
        let ptp = ptp_guard.0.take().expect("set at bring-up");
        Ok(Self {
            renderer: cfg.renderer,
            rtsp,
            volume_pct: AtomicU32::new(cfg.initial_volume.unwrap_or(100)),
            stop_flag,
            dead,
            faults,
            volume,
            resend_stats,
            sender_handle: Some(sender),
            timing_handle: None,
            sync_handle: None,
            resend_handle: None,
            event_handle: None,
            feedback_handle,
            volume_handle,
            ptp_session: Some(ptp),
            buffered_flush: None,
            data_stream: None,
            _audio_socket: None,
            followers: followers.into_iter().map(|(_, c)| c).collect(),
            extra_handles,
        })
    }

    /// True once a background thread flagged the session dead (dropped
    /// receiver). Polled by the app watchdog for auto-reconnect.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
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

    pub fn stop(mut self) {
        info!("AirPlay 2: stopping session to {}", self.renderer.friendly_name);
        self.stop_flag.store(true, Ordering::Release);
        // Unblock a buffered sender wedged in a full-buffer TCP write —
        // without this the join below can hang forever on a receiver
        // that stopped consuming, freezing the whole switch-speaker flow.
        if let Some(s) = &self.data_stream {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        for h in [
            self.sender_handle.take(),
            self.timing_handle.take(),
            self.sync_handle.take(),
            self.resend_handle.take(),
            self.event_handle.take(),
            self.feedback_handle.take(),
            self.volume_handle.take(),
        ]
        .into_iter()
        .flatten()
        .chain(self.extra_handles.drain(..))
        {
            let _ = h.join();
        }
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

impl Drop for AirPlay2Session {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
    }
}

/// Sets the stop flag when dropped (unless cleared), so the threads of a
/// bring-up that fails partway exit instead of lingering.
struct StopOnDrop(Option<Arc<AtomicBool>>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            f.store(true, Ordering::Release);
        }
    }
}

/// Stops and joins the PTP master when dropped (unless taken), so a
/// failed bring-up releases ports 319/320 before a retry binds them.
struct PtpGuard(Option<PtpMaster>);

impl Drop for PtpGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            p.stop();
        }
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

/// Pair one receiver on its RTSP connection: `pair-verify` with stored
/// credentials when there are some, transient pairing otherwise. Returns
/// the 32-byte audio key.
fn pair_member(
    rtsp: &mut Ap2Rtsp,
    creds: Option<PairingCredentials>,
    name: &str,
) -> std::result::Result<[u8; 32], Ap2StartError> {
    // Pairing: a PIN-paired receiver (Apple TV with access control)
    // gets pair-verify from the stored long-term keys; everything else
    // gets transient pairing. A transient 470 means the receiver *needs*
    // PIN pairing but we have no stored keys — surface NeedsPin so the
    // app can run the one-time PIN ceremony. A pair-verify REJECTION
    // (response received, credentials refused) surfaces as
    // VerifyRejected so the app clears the stale keys; a mere transport
    // failure stays a generic error and the keys survive.
    let key = if let Some(creds) = creds {
        info!("AirPlay 2: verifying stored pairing with {}", name);
        match rtsp.pair_verify(&creds) {
            Ok(key) => key,
            Err(crate::airplay::ap2_rtsp::PairVerifyError::Rejected(e)) => {
                warn!(
                    "AirPlay 2: {} rejected the stored pairing ({:#}) — it will be \
                     cleared for re-pairing",
                    name, e
                );
                return Err(Ap2StartError::VerifyRejected(e));
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

struct BufferedSenderConfig {
    stream: TcpStream,
    audio_key: [u8; 32],
    initial_seq: u16,
    initial_rtptime: u32,
    ssrc: u32,
    samples_rx: Receiver<PcmFrame>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
    current_rtptime: Arc<AtomicU32>,
    /// Last RTP sequence number sent — read at stop() for FLUSHBUFFERED.
    last_seq: Arc<AtomicU32>,
    /// Shared RTSP connection — the sender anchors on it at first audio.
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    /// Both PTP timelines (ours + the receiver's, once locked).
    timeline: PtpTimeline,
    /// Negotiated payload codec (must match the SETUP's ct/spf).
    codec: BufferedCodecKind,
    faults: FaultSlot,
}

/// Send SETRATEANCHORTIME for the buffered stream: "rtpTime plays at
/// (timeline now + lead)". Prefers the RECEIVER's timeline once the PTP
/// layer has locked onto its Sync/Follow_Up stream (field-tested: Sonos
/// refuses anchors on any other clock); falls back to our own timeline
/// for receivers that follow the sender instead. Anchors are rounded up
/// to a whole second so networkTimeFrac is 0.
fn try_anchor_buffered(rtsp: &Arc<Mutex<Ap2Rtsp>>, timeline: &PtpTimeline, rtp_time: u32) -> Result<u64> {
    // Take the connection first: another request (volume, read-back) may
    // hold it for a while, and an anchor computed before that wait could
    // already be in the past when it is sent.
    let mut conn = rtsp.lock().unwrap();
    let (timeline_id, base_ns) = match timeline.receiver_now_ns() {
        Some((id, now)) => (id, now),
        None => (timeline.clock_id, timeline.our_now_ns()),
    };
    let anchor_ns = (base_ns + ANCHOR_LEAD_NS).div_ceil(1_000_000_000) * 1_000_000_000;
    conn.set_rate_anchor_time(1, rtp_time, anchor_ns, timeline_id)?;
    Ok(timeline_id)
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
        "AirPlay 2 buffered sender → {:?} (seq={}, rtptime={})",
        cfg.stream.peer_addr().ok(),
        cfg.initial_seq,
        cfg.initial_rtptime
    );
    let spf = cfg.codec.spf();
    let mut sealer = AudioSealer::new(&cfg.audio_key, cfg.initial_seq);
    let mut seq = cfg.initial_seq;
    let mut rtptime = cfg.initial_rtptime;
    let mut packet_count: u64 = 0;
    let mut ring: Vec<i16> = Vec::with_capacity(spf * 2);

    // The AAC encoder must live on this thread (COM); the session already
    // probed availability before negotiating ct=4 in SETUP.
    #[cfg(windows)]
    let mut aac_encoder = if cfg.codec == BufferedCodecKind::Aac {
        match crate::airplay::aac_mf::AacEncoder::new() {
            Ok(enc) => Some(enc),
            Err(e) => {
                warn!("AirPlay 2: AAC encoder init failed on sender thread: {e:#}");
                return;
            }
        }
    } else {
        None
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
                    Err(e) => {
                        warn!("AirPlay 2 anchor failed (will retry): {:#}", e);
                    }
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
            // One spf-sized PCM chunk → zero or more payload frames (the
            // AAC MFT buffers a frame or two before its first output; ALAC
            // is always 1:1).
            let payloads: Vec<Vec<u8>> = match cfg.codec {
                BufferedCodecKind::Alac => vec![build_uncompressed_alac_frame(&pkt_samples)],
                #[cfg(windows)]
                BufferedCodecKind::Aac => match aac_encoder.as_mut().unwrap().encode(&pkt_samples) {
                    Ok(frames) => frames,
                    Err(e) => {
                        warn!("AirPlay 2: AAC encode failed: {e:#}");
                        return;
                    }
                },
                #[cfg(not(windows))]
                BufferedCodecKind::Aac => unreachable!("AAC is only negotiated on Windows"),
            };

            for payload in payloads {
                let header = ap2_rtp_header(seq, rtptime, cfg.ssrc, packet_count == 0);
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
                    info!(
                        "AirPlay 2: buffered {} audio flowing — first packet ({} bytes framed)",
                        cfg.codec.label(),
                        framed.len()
                    );
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
        };
        assert_eq!(common_codec(&[with_cn(vec![0, 1, 2, 3]), with_cn(vec![])]), Some(RealtimeCodec::Pcm));
        assert_eq!(common_codec(&[with_cn(vec![0, 1]), with_cn(vec![1])]), Some(RealtimeCodec::Alac));
        assert_eq!(common_codec(&[with_cn(vec![0]), with_cn(vec![1])]), None);
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
