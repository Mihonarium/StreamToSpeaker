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
use crate::airplay::ap2_crypto::seal_audio;
use crate::airplay::ap2_ptp::{spawn_ptp_master, PtpMaster, PtpTimeline};
use crate::airplay::ap2_rtsp::Ap2Rtsp;
use crate::airplay::discovery::AirPlayRenderer;
use crate::airplay::rtp::{bind_udp, random_initial_rtptime, random_initial_seq, random_ssrc, FRAMES_PER_PACKET};
use crate::airplay::session::volume_pct_to_raop_db;
use crate::airplay::timing::{
    spawn_resend_responder, spawn_sync_sender, spawn_sync_sender_ptp, spawn_timing_responder,
    ResendBuffer,
};
use crate::http_server::PcmFrame;
use crate::WIRE_SAMPLE_RATE;

/// Default AirPlay 2 RTSP port if the device didn't advertise one.
const DEFAULT_AIRPLAY_PORT: u16 = 7000;
/// Receiver playback latency in samples for the sync anchor (= latencyMin).
const DEFAULT_LATENCY_SAMPLES: u32 = 11025;
/// Recently-sent packets retained for retransmit (~4 s at 44.1 kHz).
const RESEND_BUFFER_PACKETS: usize = 512;
/// How far in the future the buffered stream's SETRATEANCHORTIME anchor is
/// placed — the receiver buffers packets until this point, absorbing
/// startup jitter.
const ANCHOR_LEAD_NS: u64 = 500_000_000;
/// Cadence of the /feedback keepalive iOS senders emit.
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);

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
}

/// A live AirPlay 2 session.
pub struct AirPlay2Session {
    pub renderer: AirPlayRenderer,
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    stop_flag: Arc<AtomicBool>,
    sender_handle: Option<JoinHandle<()>>,
    timing_handle: Option<JoinHandle<()>>,
    sync_handle: Option<JoinHandle<()>>,
    resend_handle: Option<JoinHandle<()>>,
    event_handle: Option<JoinHandle<()>>,
    feedback_handle: Option<JoinHandle<()>>,
    ptp_session: Option<PtpMaster>,
    /// (last sent seq, current rtptime) for the buffered stream — read at
    /// stop() to send the spec's FLUSHBUFFERED before TEARDOWN. None for
    /// realtime sessions.
    buffered_flush: Option<(Arc<AtomicU32>, Arc<AtomicU32>)>,
    /// Clone of the buffered data TCP stream — stop() shuts it down to
    /// unblock a sender wedged in a full-buffer write before joining it.
    data_stream: Option<TcpStream>,
    _audio_socket: UdpSocket,
}

impl AirPlay2Session {
    pub fn start(cfg: AirPlay2SessionConfig) -> Result<Self> {
        let port = cfg.renderer.airplay_port.unwrap_or(DEFAULT_AIRPLAY_PORT);
        info!(
            "AirPlay 2: starting session to {} ({}:{})",
            cfg.renderer.friendly_name, cfg.renderer.ip, port
        );

        // UDP sockets for audio (out), control (sync out), timing (responder).
        let audio_socket = bind_udp(cfg.local_ip).context("bind AP2 audio UDP")?;
        let control_socket = bind_udp(cfg.local_ip).context("bind AP2 control UDP")?;
        let timing_socket = bind_udp(cfg.local_ip).context("bind AP2 timing UDP")?;
        let control_port = control_socket.local_addr()?.port();
        let timing_port = timing_socket.local_addr()?.port();

        let mut rtsp = Ap2Rtsp::connect(cfg.renderer.ip, port, cfg.local_ip, Duration::from_secs(5))
            .context("AirPlay 2 RTSP connect")?;

        // Canonical opener — iOS sends GET /info before pairing. Some
        // receivers initialise per-connection state on it; harmless
        // everywhere else, so failure is non-fatal.
        if let Err(e) = rtsp.get_info() {
            warn!("AirPlay 2 GET /info failed (continuing): {:#}", e);
        }

        let audio_key = rtsp
            .pair_setup_transient()
            .context("AirPlay 2 HomeKit transient pairing")?;
        info!("AirPlay 2: paired with {}", cfg.renderer.friendly_name);

        let stop_flag = Arc::new(AtomicBool::new(false));

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
            let ptp = spawn_ptp_master(cfg.renderer.ip, cfg.local_ip, cfg.renderer.friendly_name.clone())
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
        // until the sender has a TCP connection to its eventPort. We don't
        // process events — just keep it open.
        let event_handle = spawn_event_channel(
            cfg.renderer.ip,
            event_port,
            stop_flag.clone(),
            cfg.renderer.friendly_name.clone(),
        );

        // Stream kind: buffered (type 103, TCP — what iOS actually uses,
        // and seemingly the only kind current Sonos firmware truly plays)
        // when the receiver advertises bit 40 and we're on PTP; realtime
        // (type 96, UDP) otherwise. Codec: AAC-LC via the Windows-provided
        // Media Foundation encoder — iOS's buffered codec, the only one
        // field-proven on Sonos — with ALAC as fallback (no encoder needed)
        // and realtime as the last resort. Every rejection is visible.
        let want_buffered = use_ptp && cfg.renderer.supports_buffered_audio() && !cfg.prefer_realtime;
        if cfg.prefer_realtime {
            info!("AirPlay 2: prefer_realtime_airplay set — using the realtime stream");
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
                    rtsp.setup_stream_buffered(&audio_key, control_port, 4, 1024, 0x400000)
                }
                BufferedCodecKind::Alac => {
                    rtsp.setup_stream_buffered(&audio_key, control_port, 2, 352, 0x40000)
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
                                .setup_stream(&audio_key, control_port)
                                .context("AP2 SETUP(stream, realtime fallback)")?;
                            (p, false)
                        }
                    }
                }
                Err(e) => {
                    warn!("AirPlay 2: buffered SETUP rejected ({e:#}); falling back to realtime");
                    let p = rtsp
                        .setup_stream(&audio_key, control_port)
                        .context("AP2 SETUP(stream, realtime fallback)")?;
                    (p, false)
                }
            }
        } else {
            let p = rtsp
                .setup_stream(&audio_key, control_port)
                .context("AP2 SETUP(stream)")?;
            (p, false)
        };
        debug!("AirPlay 2: receiver data port {}, control port {}", ports.data, ports.control);

        if use_ptp {
            // Hand the receiver the PTP peer address list. OwnTone's order:
            // receiver's address first, then the sender's.
            if let Err(e) = rtsp.set_peers(&[cfg.renderer.ip, cfg.local_ip]) {
                warn!("AirPlay 2 SETPEERS failed (PTP may not lock): {}", e);
            }
        }

        rtsp.record().context("AP2 RECORD")?;

        if let Some(vol) = cfg.initial_volume {
            if let Err(e) = rtsp.set_volume(volume_pct_to_raop_db(vol)) {
                warn!("AirPlay 2 initial volume failed: {}", e);
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
        let resend = ResendBuffer::new(RESEND_BUFFER_PACKETS);

        // The control socket carries outbound sync packets and inbound
        // resend requests — clone it so both threads can use it.
        let control_for_resend = control_socket
            .try_clone()
            .context("clone AP2 control socket")?;

        let mut buffered_flush = None;
        let mut data_stream: Option<TcpStream> = None;
        let (sender_handle, timing_handle, sync_handle, resend_handle) = if buffered {
            // Buffered: audio goes over ONE TCP connection to the data
            // port; playback is anchored by SETRATEANCHORTIME on our PTP
            // timeline. No sync packets, no resend (TCP is reliable), no
            // NTP timing responder.
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
            })?;
            info!(
                "AirPlay 2: buffered {} stream armed — will anchor at first audio",
                codec.label()
            );
            drop(timing_socket);
            drop(control_socket);
            drop(control_for_resend);
            drop(resend);
            (sender, None, None, None)
        } else {
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
                current_rtptime: current_rtptime.clone(),
                resend: resend.clone(),
            })?;

            let sync_addr = SocketAddr::new(cfg.renderer.ip, ports.control);
            let (timing_handle, sync_handle) = if use_ptp {
                let ptp = ptp_session.as_ref().unwrap();
                let sync = spawn_sync_sender_ptp(
                    control_socket,
                    sync_addr,
                    current_rtptime,
                    DEFAULT_LATENCY_SAMPLES,
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
                    current_rtptime,
                    DEFAULT_LATENCY_SAMPLES,
                    stop_flag.clone(),
                    cfg.renderer.friendly_name.clone(),
                )
                .context("spawning AP2 sync sender")?;
                (Some(timing), Some(sync))
            };

            let resend_handle = spawn_resend_responder(
                control_for_resend,
                sync_addr,
                resend,
                stop_flag.clone(),
                cfg.renderer.friendly_name.clone(),
            )
            .context("spawning AP2 resend responder")?;
            (sender, timing_handle, sync_handle, Some(resend_handle))
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
            cfg.renderer.friendly_name.clone(),
        );

        Ok(Self {
            renderer: cfg.renderer,
            rtsp,
            stop_flag,
            sender_handle: Some(sender_handle),
            timing_handle,
            sync_handle,
            resend_handle,
            event_handle,
            feedback_handle,
            ptp_session,
            buffered_flush,
            data_stream,
            _audio_socket: audio_socket,
        })
    }

    pub fn set_volume_pct(&self, vol: u32) -> Result<()> {
        self.rtsp.lock().unwrap().set_volume(volume_pct_to_raop_db(vol))
    }

    pub fn set_mute(&self, muted: bool) -> Result<()> {
        let db = if muted { -144.0 } else { 0.0 };
        self.rtsp.lock().unwrap().set_volume(db)
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
        ]
        .into_iter()
        .flatten()
        {
            let _ = h.join();
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
            let mut buf = [0u8; 1024];
            while !stop_flag.load(Ordering::Acquire) {
                match stream.read(&mut buf) {
                    Ok(0) => break, // receiver closed the channel
                    Ok(_) => {}     // discard events
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
            }
            debug!("AirPlay 2 event channel closed");
        })
        .ok()
}

/// POST /feedback every ~2 s until the session stops. Failures downgrade
/// to debug after the first warn — a dropped keepalive shouldn't spam.
fn spawn_feedback_keepalive(
    rtsp: Arc<Mutex<Ap2Rtsp>>,
    stop_flag: Arc<AtomicBool>,
    receiver_name: String,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-feedback:{}", receiver_name))
        .spawn(move || {
            let mut warned = false;
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
}

/// Send SETRATEANCHORTIME for the buffered stream: "rtpTime plays at
/// (timeline now + lead)". Prefers the RECEIVER's timeline once the PTP
/// layer has locked onto its Sync/Follow_Up stream (field-tested: Sonos
/// refuses anchors on any other clock); falls back to our own timeline
/// for receivers that follow the sender instead. Anchors are rounded up
/// to a whole second so networkTimeFrac is 0.
fn try_anchor_buffered(rtsp: &Arc<Mutex<Ap2Rtsp>>, timeline: &PtpTimeline, rtp_time: u32) -> Result<u64> {
    let (timeline_id, base_ns) = match timeline.receiver_now_ns() {
        Some((id, now)) => (id, now),
        None => (timeline.clock_id, timeline.our_now_ns()),
    };
    let anchor_ns = (base_ns + ANCHOR_LEAD_NS).div_ceil(1_000_000_000) * 1_000_000_000;
    rtsp.lock()
        .unwrap()
        .set_rate_anchor_time(1, rtp_time, anchor_ns, timeline_id)?;
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
                let sealed = seal_audio(&cfg.audio_key, &header, seq, &payload);
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
                    warn!("AirPlay 2 buffered send failed (receiver stopped reading?): {}", e);
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
    resend: Arc<ResendBuffer>,
}

fn spawn_ap2_sender(cfg: Ap2SenderConfig) -> Result<JoinHandle<()>> {
    let name = format!("stream-to-speaker-ap2-rtp:{}", cfg.receiver_name);
    Ok(std::thread::Builder::new().name(name).spawn(move || run_ap2_sender(cfg))?)
}

fn run_ap2_sender(cfg: Ap2SenderConfig) {
    info!(
        "AirPlay 2 RTP sender → {} (seq={}, rtptime={})",
        cfg.receiver_addr, cfg.initial_seq, cfg.initial_rtptime
    );
    let mut seq = cfg.initial_seq;
    let mut rtptime = cfg.initial_rtptime;
    let mut packet_count: u64 = 0;
    let mut ring: Vec<i16> = Vec::with_capacity(FRAMES_PER_PACKET * 2);

    let start = Instant::now();
    let packet_duration =
        Duration::from_nanos((FRAMES_PER_PACKET as u64 * 1_000_000_000) / WIRE_SAMPLE_RATE as u64);
    let mut idle_warned = false;

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
                // No audio from the source. If nothing has ever arrived,
                // the problem is upstream (nothing playing / wrong default
                // device), not the AirPlay stream — say so once.
                if packet_count == 0 && !idle_warned && start.elapsed() > Duration::from_secs(3) {
                    warn!(
                        "AirPlay 2: no audio from the source after 3s — is something playing with \
                         Stream To Speaker selected as the Windows output device?"
                    );
                    idle_warned = true;
                }
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }

        let mut packets_this_round = 0u32;
        while ring.len() >= FRAMES_PER_PACKET * 2 {
            let pkt_samples: Vec<i16> = ring.drain(..FRAMES_PER_PACKET * 2).collect();
            let alac = build_uncompressed_alac_frame(&pkt_samples);

            let header = ap2_rtp_header(seq, rtptime, cfg.ssrc, packet_count == 0);
            let sealed = seal_audio(&cfg.audio_key, &header, seq, &alac);
            let mut packet = Vec::with_capacity(12 + sealed.len());
            packet.extend_from_slice(&header);
            packet.extend_from_slice(&sealed);

            let deadline = start + packet_duration.saturating_mul((packet_count + 1) as u32);
            let now = Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            }

            if let Err(e) = cfg.audio_socket.send_to(&packet, cfg.receiver_addr) {
                warn!("AirPlay 2 RTP send failed: {}", e);
                return;
            }
            // Retain for retransmit on a resend request.
            cfg.resend.record(seq, &packet);
            if packet_count == 0 {
                info!(
                    "AirPlay 2: audio flowing — first packet ({} bytes) sent to {}",
                    packet.len(),
                    cfg.receiver_addr
                );
            }

            seq = seq.wrapping_add(1);
            rtptime = rtptime.wrapping_add(FRAMES_PER_PACKET as u32);
            cfg.current_rtptime.store(rtptime, Ordering::Release);
            packet_count += 1;
            if packet_count % 500 == 0 {
                debug!(
                    "AirPlay 2: {} audio packets sent ({} s)",
                    packet_count,
                    packet_count * FRAMES_PER_PACKET as u64 / WIRE_SAMPLE_RATE as u64
                );
            }
            packets_this_round += 1;
            if packets_this_round > 32 {
                break;
            }
        }
    }
    info!("AirPlay 2 RTP sender stopped after {} packets", packet_count);
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
