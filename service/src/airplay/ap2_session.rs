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
use crate::airplay::ap2_ptp::{spawn_ptp_master, PtpMaster};
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
                .setup_timing_ptp(ptp.clock_id, &ptp.clock_uuid)
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
        // (type 96, UDP) otherwise. A rejected buffered SETUP falls back
        // to realtime *visibly* — unlike the silent failure modes.
        let want_buffered = use_ptp && cfg.renderer.supports_buffered_audio();
        let (ports, buffered) = if want_buffered {
            match rtsp.setup_stream_buffered(&audio_key, control_port) {
                Ok(p) => {
                    info!("AirPlay 2: buffered stream accepted (type 103/ALAC, TCP data port {})", p.data);
                    (p, true)
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

        let (sender_handle, timing_handle, sync_handle, resend_handle) = if buffered {
            // Buffered: audio goes over ONE TCP connection to the data
            // port; playback is anchored by SETRATEANCHORTIME on our PTP
            // timeline. No sync packets, no resend (TCP is reliable), no
            // NTP timing responder.
            let data_addr = SocketAddr::new(cfg.renderer.ip, ports.data);
            let stream = TcpStream::connect_timeout(&data_addr, Duration::from_secs(3))
                .with_context(|| format!("connecting AP2 buffered data TCP to {}", data_addr))?;
            stream.set_nodelay(true).ok();
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
            })?;

            // Anchor: initial_rtptime plays ANCHOR_LEAD from now on our
            // grandmaster timeline. Sent after the sender is running so the
            // receiver's buffer is filling during the lead.
            let ptp = ptp_session.as_ref().unwrap();
            let anchor_ns = ptp.clock.now_ns() + ANCHOR_LEAD_NS;
            rtsp.set_rate_anchor_time(1, initial_rtptime, anchor_ns, ptp.clock_id)
                .context("AP2 SETRATEANCHORTIME")?;
            info!(
                "AirPlay 2: anchored — rtpTime {} plays at +{} ms on timeline {:#018x}",
                initial_rtptime,
                ANCHOR_LEAD_NS / 1_000_000,
                ptp.clock_id
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
                    ptp.clock.clone(),
                    ptp.clock_id,
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
        let rtsp = Arc::new(Mutex::new(rtsp));
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
            let framed = frame_buffered_packet(&packet);

            // Same wall-clock pacing as realtime — the receiver buffers,
            // so exact pacing isn't critical, but matching the sample rate
            // keeps our send queue and its buffer bounded.
            let deadline = start + packet_duration.saturating_mul((packet_count + 1) as u32);
            let now = Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            }

            if let Err(e) = cfg.stream.write_all(&framed) {
                warn!("AirPlay 2 buffered send failed: {}", e);
                return;
            }
            if packet_count == 0 {
                info!(
                    "AirPlay 2: buffered audio flowing — first packet ({} bytes framed)",
                    framed.len()
                );
            }

            seq = seq.wrapping_add(1);
            rtptime = rtptime.wrapping_add(FRAMES_PER_PACKET as u32);
            cfg.current_rtptime.store(rtptime, Ordering::Release);
            packet_count += 1;
            if packet_count % 500 == 0 {
                debug!(
                    "AirPlay 2: {} buffered packets sent ({} s)",
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
