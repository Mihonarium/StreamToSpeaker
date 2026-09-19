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
use crate::airplay::ap2_crypto::seal_audio;
use crate::airplay::ap2_ptp::{spawn_ptp_master_multi, PtpMaster, PtpTimeline};
use crate::airplay::ap2_rtsp::{Ap2Rtsp, StreamPorts, TransientOutcome};
use crate::airplay::discovery::AirPlayRenderer;
use crate::airplay::hap_pairing::PairingCredentials;
use crate::airplay::rtp::{bind_udp, random_initial_rtptime, random_initial_seq, random_ssrc, FRAMES_PER_PACKET};
use crate::airplay::session::{mute_db, volume_pct_to_raop_db};
use crate::airplay::timing::{
    spawn_resend_responder, spawn_sync_sender, spawn_sync_sender_ptp, spawn_timing_responder,
    ResendBuffer,
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

pub struct AirPlay2SessionConfig {
    /// The receiver — for a stereo pair, its first half.
    pub renderer: AirPlayRenderer,
    /// The other half (halves) of a stereo pair. Each gets its own paired
    /// RTSP session, but all of them share ONE packetiser (identical RTP
    /// seq/timestamps), ONE PTP grandmaster clock and ONE anchor, so
    /// they play in lock-step; every half receives the full stereo
    /// stream and picks its own channel (owntone#1413: "each speaker
    /// knows which channel to play but all speakers receive the same
    /// stream"). Empty for a single receiver.
    pub partners: Vec<AirPlayRenderer>,
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

/// A live AirPlay 2 session: one receiver, or the halves of a stereo
/// pair driven in lock-step (see [`AirPlay2SessionConfig::partners`]).
pub struct AirPlay2Session {
    /// The receiver, or the first half of the pair (the row's device).
    pub renderer: AirPlayRenderer,
    /// Every receiver in the session, `renderer` first.
    pub members: Vec<AirPlayRenderer>,
    /// One RTSP connection per member, same order as `members`.
    rtsps: Vec<Arc<Mutex<Ap2Rtsp>>>,
    /// Last volume (0..=100) pushed to the receivers; restored on unmute.
    volume_pct: AtomicU32,
    stop_flag: Arc<AtomicBool>,
    /// Set by background threads when the session has demonstrably died
    /// (audio send error, buffered TCP write failure, repeated /feedback
    /// failures). Polled by the app watchdog for auto-reconnect. Any one
    /// member dying kills the whole session: a pair reconnects as a pair.
    dead: Arc<AtomicBool>,
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

/// One receiver's connection state while the session is brought up.
struct Half {
    renderer: AirPlayRenderer,
    rtsp: Ap2Rtsp,
    audio_key: [u8; 32],
    audio_socket: UdpSocket,
    control_socket: UdpSocket,
    timing_socket: UdpSocket,
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

    let mut rtsp = Ap2Rtsp::connect(renderer.ip, port, local_ip, Duration::from_secs(5))
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
                return Err(Ap2StartError::NeedsPin);
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
        timing_socket,
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

/// What the UI calls the session: a single receiver's name; for a pair
/// its group name (`gpn`), else "Left + Right".
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

/// The SETPEERS body for one member: that receiver, then the sender —
/// the per-session list OwnTone (`payload_make_setpeers`) and airplay-cli
/// send, with which both drive stereo pairs. The other half is not named.
fn setpeers_list(receiver_ip: IpAddr, local_ip: IpAddr) -> [IpAddr; 2] {
    [receiver_ip, local_ip]
}

impl AirPlay2Session {
    pub fn start(cfg: AirPlay2SessionConfig) -> std::result::Result<Self, Ap2StartError> {
        let members: Vec<AirPlayRenderer> = std::iter::once(cfg.renderer.clone())
            .chain(cfg.partners.iter().cloned())
            .collect();
        let paired = members.len() > 1;
        let label = session_label(&members);
        if paired {
            info!(
                "AirPlay 2: stereo pair {} — one session per half, one shared clock and anchor",
                label
            );
        }

        // Connect + pair every member first: a pair either comes up
        // whole or not at all (an Ap2Rtsp that paired tears itself down
        // on drop, so an early return leaves no half-open sessions).
        let mut halves: Vec<Half> = Vec::with_capacity(members.len());
        for r in &members {
            halves.push(connect_and_pair(r, cfg.local_ip, cfg.pairing_creds.get(&r.stable_id()))?);
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let resend_stats = Arc::new(crate::airplay::timing::ResendStats::default());

        // Timing-protocol choice: receivers advertising SupportsPTP (bit 41)
        // get the full PTP path — field-tested on a SYMFONISK whose current
        // firmware stalls the stream SETUP under timingProtocol=NTP (NTP
        // appears as vestigial as its RAOP). For PTP **we are the
        // grandmaster** — start the master first so its clock identity goes
        // into the SETUP payload and the clock (plus Announce/Signaling) is
        // already being served when the receiver processes it. A pair
        // shares the one master (it owns UDP 319/320) and must agree on
        // the protocol.
        let use_ptp = cfg.renderer.expects_ptp();
        if let Some(odd) = members.iter().find(|r| r.expects_ptp() != use_ptp) {
            return Err(anyhow::anyhow!(
                "{} and {} disagree on PTP timing; a stereo pair needs one shared clock",
                cfg.renderer.friendly_name,
                odd.friendly_name
            )
            .into());
        }
        info!(
            "AirPlay 2: timing for {} = {} (model {:?})",
            label,
            if use_ptp { "PTP (we serve as grandmaster)" } else { "NTP" },
            cfg.renderer.model.as_deref().unwrap_or("?"),
        );
        let ptp_session = if use_ptp {
            let ips: Vec<IpAddr> = members.iter().map(|r| r.ip).collect();
            Some(
                spawn_ptp_master_multi(ips, cfg.local_ip, label.clone())
                    .context("starting AP2 PTP master")?,
            )
        } else {
            None
        };
        for h in &mut halves {
            let ts = match &ptp_session {
                Some(ptp) => h
                    .rtsp
                    .setup_timing_ptp(ptp.timeline.clock_id, &ptp.clock_uuid)
                    .with_context(|| format!("AP2 SETUP(timing/PTP) on {}", h.renderer.friendly_name))?,
                None => h
                    .rtsp
                    .setup_timing_ntp(h.timing_port)
                    .with_context(|| format!("AP2 SETUP(timing/NTP) on {}", h.renderer.friendly_name))?,
            };
            h.event_port = ts.event_port;
        }

        // Open the event channels: the receiver withholds its RECORD
        // response until the sender has a TCP connection to its eventPort.
        // We don't process events — just keep them open.
        let mut threads: Vec<JoinHandle<()>> = Vec::new();
        for h in &halves {
            threads.extend(spawn_event_channel(
                h.renderer.ip,
                h.event_port,
                stop_flag.clone(),
                h.renderer.friendly_name.clone(),
            ));
        }

        // Stream kind: buffered (type 103, TCP — what iOS actually uses,
        // and seemingly the only kind current Sonos firmware truly plays)
        // when the receiver advertises bit 40 and we're on PTP; realtime
        // (type 96, UDP) otherwise. Codec: AAC-LC via the Windows-provided
        // Media Foundation encoder — iOS's buffered codec, the only one
        // field-proven on Sonos — with ALAC as fallback (no encoder needed)
        // and realtime as the last resort. Every rejection is visible. One
        // packetiser feeds every member, so the kind and codec are decided
        // once: the first member negotiates with the full fallback chain,
        // the others must accept the same.
        let latency_samples = crate::airplay::timing::latency_ms_to_samples(cfg.latency_ms);
        let low_latency = cfg.latency_ms < LOW_LATENCY_REALTIME_MS;
        let all_buffered = members.iter().all(|r| r.supports_buffered_audio());
        let want_buffered = use_ptp && all_buffered && !cfg.prefer_realtime && !low_latency;
        if cfg.prefer_realtime {
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
        let attempt = |h: &mut Half, k: BufferedCodecKind| match k {
            BufferedCodecKind::Aac => h.rtsp.setup_stream_buffered(
                &h.audio_key,
                h.control_port,
                4,
                1024,
                0x400000,
                DEFAULT_LATENCY_SAMPLES,
            ),
            BufferedCodecKind::Alac => h.rtsp.setup_stream_buffered(
                &h.audio_key,
                h.control_port,
                2,
                352,
                0x40000,
                DEFAULT_LATENCY_SAMPLES,
            ),
        };
        let realtime = |h: &mut Half| h.rtsp.setup_stream(&h.audio_key, h.control_port, latency_samples);
        let buffered = {
            let first = &mut halves[0];
            let (ports, buffered) = if want_buffered {
                match attempt(first, codec) {
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
                        match attempt(first, codec) {
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
        for h in halves.iter_mut().skip(1) {
            // Same kind and codec as the first half, or the pair can't
            // share one packetiser — no per-half fallback.
            let p = if buffered { attempt(h, codec) } else { realtime(h) }.with_context(|| {
                format!(
                    "AP2 SETUP(stream {}) on {} — every half of a pair must accept the stream kind \
                     the first half negotiated",
                    if buffered { format!("buffered/{}", codec.label()) } else { "realtime".to_string() },
                    h.renderer.friendly_name
                )
            })?;
            h.ports = Some(p);
        }
        for h in &halves {
            let ports = h.ports.as_ref().expect("every half negotiated a stream");
            debug!(
                "AirPlay 2: {} data port {}, control port {}",
                h.renderer.friendly_name, ports.data, ports.control
            );
        }

        // Open the buffered data connections straight after the stream
        // SETUP — before SETPEERS/RECORD. Receivers hold connection state
        // per phase (the event channel must exist before RECORD, the
        // anchor only works at first audio), and real senders connect the
        // data socket early too.
        let mut data_streams: Vec<TcpStream> = Vec::new();
        if buffered {
            for h in &mut halves {
                let data_addr = SocketAddr::new(h.renderer.ip, h.ports.as_ref().unwrap().data);
                let stream = TcpStream::connect_timeout(&data_addr, Duration::from_secs(3))
                    .with_context(|| format!("connecting AP2 buffered data TCP to {}", data_addr))?;
                stream.set_nodelay(true).ok();
                // Without a write timeout, a receiver that stops consuming
                // fills the socket buffer and write_all blocks forever —
                // which wedges stop() on the sender join and leaves the
                // whole app stuck "Connecting…" with TEARDOWN never sent.
                stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
                data_streams.extend(stream.try_clone().ok());
                h.data = Some(stream);
            }
        }

        if use_ptp {
            // Hand each receiver its PTP peer address list: itself, then
            // the sender (OwnTone's and airplay-cli's order — both drive
            // stereo pairs with exactly this per-session list; the other
            // half is not named).
            for h in &mut halves {
                if let Err(e) = h.rtsp.set_peers(&setpeers_list(h.renderer.ip, cfg.local_ip)) {
                    warn!(
                        "AirPlay 2 SETPEERS on {} failed (PTP may not lock): {}",
                        h.renderer.friendly_name, e
                    );
                }
            }
        }

        for h in &mut halves {
            h.rtsp
                .record()
                .with_context(|| format!("AP2 RECORD on {}", h.renderer.friendly_name))?;
        }

        if let Some(vol) = cfg.initial_volume {
            for h in &mut halves {
                if let Err(e) = h.rtsp.set_volume(volume_pct_to_raop_db(vol)) {
                    warn!("AirPlay 2 initial volume on {} failed: {}", h.renderer.friendly_name, e);
                }
            }
        }

        // From here the RTSP connections are shared: the buffered sender
        // anchors on them at first audio, the feedback keepalives post to
        // them, and volume changes arrive from arbitrary threads.
        let mut rtsps: Vec<Arc<Mutex<Ap2Rtsp>>> = Vec::with_capacity(halves.len());
        let mut audio_sockets: Vec<UdpSocket> = Vec::with_capacity(halves.len());
        let mut outlets: Vec<Outlet> = Vec::with_capacity(halves.len());
        let mut controls: Vec<(UdpSocket, SocketAddr, String)> = Vec::with_capacity(halves.len());
        let mut timings: Vec<(UdpSocket, String)> = Vec::with_capacity(halves.len());
        for h in halves {
            let ports = h.ports.expect("every half negotiated a stream");
            rtsps.push(Arc::new(Mutex::new(h.rtsp)));
            controls.push((h.control_socket, SocketAddr::new(h.renderer.ip, ports.control), h.renderer.friendly_name.clone()));
            timings.push((h.timing_socket, h.renderer.friendly_name.clone()));
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
            });
            audio_sockets.push(h.audio_socket);
        }

        // Background threads. One RTP timeline for every member: the same
        // seq/rtptime on each half is what keeps a pair in lock-step.
        let initial_seq = random_initial_seq();
        let initial_rtptime = random_initial_rtptime();
        let ssrc = random_ssrc();
        let current_rtptime = Arc::new(AtomicU32::new(initial_rtptime));

        let mut buffered_flush = None;
        let sender_handle = if buffered {
            // Buffered: audio goes over the (already-connected) TCP data
            // connections; playback is anchored by SETRATEANCHORTIME. No
            // sync packets, no resend (TCP is reliable), no NTP timing
            // responder.
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
                session_dead: dead.clone(),
            })?;
            info!(
                "AirPlay 2: buffered {} stream armed — will anchor at first audio",
                codec.label()
            );
            drop(timings);
            drop(controls);
            sender
        } else {
            // Anchor before audio: both sync spawners send their initial
            // (extension-bit) sync synchronously before returning, so
            // spawning them BEFORE the audio sender guarantees the
            // receiver has a timeline anchor before the first audio
            // packet arrives — same ordering rule as the RAOP path.
            let mut resend_controls: Vec<(UdpSocket, SocketAddr, String)> = Vec::new();
            for ((control_socket, sync_addr, name), (timing_socket, _)) in controls.into_iter().zip(timings) {
                // The control socket carries outbound sync packets and
                // inbound resend requests — clone it so both threads can
                // use it.
                let control_for_resend = control_socket.try_clone().context("clone AP2 control socket")?;
                if let Some(ptp) = &ptp_session {
                    threads.push(
                        spawn_sync_sender_ptp(
                            control_socket,
                            sync_addr,
                            current_rtptime.clone(),
                            latency_samples,
                            ptp.timeline.clone(),
                            stop_flag.clone(),
                            name.clone(),
                        )
                        .context("spawning AP2 PTP sync sender")?,
                    );
                    // The NTP timing responder is unused under PTP; release its socket.
                    drop(timing_socket);
                } else {
                    threads.push(
                        spawn_timing_responder(timing_socket, stop_flag.clone(), name.clone())
                            .context("spawning AP2 timing responder")?,
                    );
                    threads.push(
                        spawn_sync_sender(
                            control_socket,
                            sync_addr,
                            current_rtptime.clone(),
                            latency_samples,
                            stop_flag.clone(),
                            name.clone(),
                        )
                        .context("spawning AP2 sync sender")?,
                    );
                }
                resend_controls.push((control_for_resend, sync_addr, name));
            }

            let resends: Vec<Arc<ResendBuffer>> =
                outlets.iter().map(|o| o.resend.clone().expect("realtime outlets keep a resend buffer")).collect();
            let sender = spawn_ap2_sender(Ap2SenderConfig {
                outlets,
                initial_seq,
                initial_rtptime,
                ssrc,
                samples_rx: cfg.samples_rx,
                stop_flag: stop_flag.clone(),
                receiver_name: label.clone(),
                current_rtptime,
                session_dead: dead.clone(),
            })?;

            for ((control_for_resend, sync_addr, name), resend) in resend_controls.into_iter().zip(resends) {
                threads.push(
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
            threads.extend(spawn_feedback_keepalive(
                rtsp.clone(),
                stop_flag.clone(),
                dead.clone(),
                r.friendly_name.clone(),
            ));
        }

        Ok(Self {
            renderer: cfg.renderer,
            members,
            rtsps,
            volume_pct: AtomicU32::new(cfg.initial_volume.unwrap_or(100)),
            stop_flag,
            dead,
            resend_stats,
            sender_handle: Some(sender_handle),
            threads,
            ptp_session,
            buffered_flush,
            data_streams,
            _audio_sockets: audio_sockets,
        })
    }

    /// True once a background thread flagged the session dead (dropped
    /// receiver). Polled by the app watchdog for auto-reconnect.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// `(resend requests, packets re-sent)` so far this session.
    pub fn resend_stats(&self) -> (u64, u64) {
        self.resend_stats.snapshot()
    }

    /// True if this session drives the halves of a stereo pair.
    pub fn is_pair(&self) -> bool {
        self.members.len() > 1
    }

    /// What the UI calls this session: the receiver's name, or for a
    /// pair its group name (`gpn`) — else "Left + Right".
    pub fn display_name(&self) -> String {
        session_display_name(&self.members)
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

/// Seal one RTP packet for every outlet: the header and payload are the
/// members' shared timeline, the ChaCha seal is per member key. Result
/// order = outlet order.
fn seal_for_outlets(outlets: &[Outlet], header: &[u8; 12], seq: u16, payload: &[u8]) -> Vec<Vec<u8>> {
    outlets
        .iter()
        .map(|o| {
            let sealed = seal_audio(&o.audio_key, header, seq, payload);
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
}

/// Open the AirPlay 2 event channel — a TCP connection to the receiver's
/// `eventPort` (from the first SETUP response). The receiver withholds its
/// RECORD response until this connection exists. We don't act on the events
/// it may send; a drain thread just keeps the socket open and discards
/// anything received until shutdown. Best-effort: a missing/unreachable
/// port logs and returns None rather than failing the session.
fn spawn_event_channel(
    receiver_ip: IpAddr,
    event_port: u16,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
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
            use std::io::Read;
            let mut stream = stream;
            let mut buf = [0u8; 2048];
            let mut chunks: u64 = 0;
            while !stop_flag.load(Ordering::Acquire) {
                match stream.read(&mut buf) {
                    Ok(0) => break, // receiver closed the channel
                    Ok(n) => {
                        // Diagnostic: receivers can send requests on this
                        // channel; if one gates playback, discarding it
                        // silently would look exactly like our silence bug.
                        chunks += 1;
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
                    Err(_) => break,
                }
            }
            debug!("AirPlay 2 event channel closed ({} chunks received)", chunks);
        })
        .ok()
}

/// POST /feedback every ~2 s until the session stops. Failures downgrade
/// to debug after the first warn — a dropped keepalive shouldn't spam.
fn spawn_feedback_keepalive(
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    stop_flag: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    receiver_name: String,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-feedback:{}", receiver_name))
        .spawn(move || {
            let mut warned = false;
            let mut consecutive_failures = 0u32;
            'outer: loop {
                // Sleep in slices so shutdown isn't delayed.
                let slices = (FEEDBACK_INTERVAL.as_millis() / 100) as u32;
                for _ in 0..slices {
                    if stop_flag.load(Ordering::Acquire) {
                        break 'outer;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if let Err(e) = rtsp.lock().unwrap().feedback() {
                    if !warned {
                        warn!("AirPlay 2 /feedback failed (continuing): {:#}", e);
                        warned = true;
                    } else {
                        debug!("AirPlay 2 /feedback failed: {:#}", e);
                    }
                    consecutive_failures += 1;
                    // Sustained /feedback failure = the encrypted control
                    // channel is gone; flag the session dead for the app
                    // watchdog.
                    if consecutive_failures >= 3 {
                        dead.store(true, Ordering::Release);
                    }
                } else {
                    consecutive_failures = 0;
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

struct BufferedSenderConfig {
    /// Every member's TCP data connection + key, first half first.
    outlets: Vec<Outlet>,
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
    session_dead: Arc<AtomicBool>,
}

/// Send SETRATEANCHORTIME for the buffered stream: "rtpTime plays at
/// (timeline now + lead)". Prefers the RECEIVER's timeline once the PTP
/// layer has locked onto its Sync/Follow_Up stream (field-tested: Sonos
/// refuses anchors on any other clock); falls back to our own timeline
/// for receivers that follow the sender instead. Anchors are rounded up
/// to a whole second so networkTimeFrac is 0. Every member gets the
/// SAME anchor (one rtpTime, one instant, one clock id) — that is what
/// makes a stereo pair start in lock-step; the PTP layer never follows a
/// receiver clock when it serves more than one receiver, so the shared
/// anchor is always on our grandmaster clock then. All-or-nothing: a
/// refusal by any member fails the anchor and the caller retries it on
/// every member (re-anchoring an accepting member is harmless — the
/// values are simply newer).
fn try_anchor_buffered(rtsps: &[Arc<Mutex<Ap2Rtsp>>], timeline: &PtpTimeline, rtp_time: u32) -> Result<u64> {
    let anchor = compute_anchor(timeline.receiver_now_ns(), timeline.clock_id, timeline.our_now_ns());
    for rtsp in rtsps {
        rtsp.lock()
            .unwrap()
            .set_rate_anchor_time(1, rtp_time, anchor.network_ns, anchor.timeline_id)?;
    }
    Ok(anchor.timeline_id)
}

/// A playback anchor shared by every member of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    timeline_id: u64,
    network_ns: u64,
}

/// The anchor for "now": on the receiver's clock when the PTP layer has
/// locked onto one, else on ours; `ANCHOR_LEAD_NS` ahead, rounded up to
/// a whole second.
fn compute_anchor(receiver_now: Option<(u64, u64)>, our_clock_id: u64, our_now_ns: u64) -> Anchor {
    let (timeline_id, base_ns) = receiver_now.unwrap_or((our_clock_id, our_now_ns));
    let network_ns = (base_ns + ANCHOR_LEAD_NS).div_ceil(1_000_000_000) * 1_000_000_000;
    Anchor { timeline_id, network_ns }
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
                match try_anchor_buffered(&cfg.rtsps, &cfg.timeline, rtptime) {
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

                // Pace to wall-clock from the anchor: the receiver plays
                // rtptime-at-anchor 0.5-1.5 s from now, so staying at the
                // sample rate keeps its buffer bounded on both sides.
                let deadline = pace_start + packet_duration.saturating_mul((packet_count + 1) as u32);
                let now = Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline - now);
                }

                // Same header + payload to every member, sealed per key.
                let packets = seal_for_outlets(&cfg.outlets, &header, seq, &payload);
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
                        cfg.session_dead.store(true, Ordering::Release);
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
    initial_seq: u16,
    initial_rtptime: u32,
    ssrc: u32,
    samples_rx: Receiver<PcmFrame>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
    current_rtptime: Arc<AtomicU32>,
    session_dead: Arc<AtomicBool>,
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
        // Same header + payload to every member, sealed per key.
        let packets = seal_for_outlets(&cfg.outlets, &header, seq, &alac);

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
                cfg.session_dead.store(true, Ordering::Release);
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
        let a = compute_anchor(None, 0x1234, 1_700_000_000);
        assert_eq!(a.timeline_id, 0x1234);
        assert_eq!(a.network_ns, 3_000_000_000);
        // Exactly on a second boundary after the lead: no extra second.
        let a = compute_anchor(None, 0x1234, 2_500_000_000);
        assert_eq!(a.network_ns, 3_000_000_000);
        // Receiver clock preferred when the PTP layer has locked onto one
        // (single-receiver Sonos behaviour); its id names the timeline.
        let a = compute_anchor(Some((0xABCD, 10_100_000_000)), 0x1234, 7);
        assert_eq!(a, Anchor { timeline_id: 0xABCD, network_ns: 11_000_000_000 });
        // The anchor is a value, computed once — every member of a pair
        // gets a byte-identical SETRATEANCHORTIME by construction.
        assert_eq!(compute_anchor(None, 9, 123), compute_anchor(None, 9, 123));
    }

    #[test]
    fn setpeers_names_that_receiver_then_us() {
        let receiver: IpAddr = "192.0.2.11".parse().unwrap();
        let other_half: IpAddr = "192.0.2.12".parse().unwrap();
        let us: IpAddr = "192.0.2.2".parse().unwrap();
        assert_eq!(setpeers_list(receiver, us), [receiver, us]);
        // The other half is NOT in this receiver's list (OwnTone /
        // airplay-cli per-session shape).
        assert!(!setpeers_list(receiver, us).contains(&other_half));
        assert_eq!(setpeers_list(other_half, us), [other_half, us]);
    }

    #[test]
    fn seal_fan_out_shares_header_and_payload_but_not_the_seal() {
        let socket = || UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let outlet = |key: u8, resend: bool| Outlet {
            name: format!("half {key}"),
            audio_key: [key; 32],
            dest: OutletDest::Udp { socket: socket(), addr },
            resend: resend.then(|| ResendBuffer::new(4)),
        };
        let outlets = vec![outlet(1, true), outlet(2, true), outlet(1, false)];
        let header = ap2_rtp_header(7, 44100, 0xDEADBEEF, true);
        let payload = build_uncompressed_alac_frame(&vec![0x0102i16; FRAMES_PER_PACKET * 2]);
        let packets = seal_for_outlets(&outlets, &header, 7, &payload);
        assert_eq!(packets.len(), 3);
        for p in &packets {
            assert_eq!(&p[..12], &header, "same RTP seq/timestamp on every member");
            assert_eq!(p.len(), 12 + payload.len() + 16 + 8, "ciphertext + tag + nonce");
        }
        assert_ne!(packets[0][12..], packets[1][12..], "different keys → different seals");
        assert_eq!(packets[0], packets[2], "same key → identical bytes (deterministic)");
        // A single outlet is exactly today's one packet.
        let single = seal_for_outlets(&outlets[..1], &header, 7, &payload);
        assert_eq!(single, vec![packets[0].clone()]);
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
