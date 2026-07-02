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
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::airplay::crypto::Cipher;
use crate::airplay::discovery::AirPlayRenderer;
use crate::airplay::rtp::{
    bind_udp, random_initial_rtptime, random_initial_seq, random_ssrc, spawn_audio_sender,
    RtpSenderConfig,
};
use crate::airplay::rtsp::RtspClient;
use crate::airplay::timing::{
    spawn_resend_responder, spawn_sync_sender, spawn_timing_responder, ResendBuffer,
};
use crate::http_server::PcmFrame;

/// Recently-sent packets retained for retransmit (~4 s at 44.1 kHz).
const RESEND_BUFFER_PACKETS: usize = 512;

/// Playback latency in samples for the sync-packet anchor. 88200 = 2 s at
/// 44.1 kHz — packet-capture verified: iTunes→Sonos sync packets carry
/// exactly `next_rtptime − 88200` (and node_airtunes2 hardcodes
/// `2 * sampling_rate`).
const DEFAULT_LATENCY_SAMPLES: u32 = 88200;

/// Configuration to spin up one AirPlay session.
pub struct AirPlaySessionConfig {
    pub renderer: AirPlayRenderer,
    /// Local IPv4 we'll bind UDP sockets to and advertise in SDP. Same
    /// IP we already use for `advertise_ip` on the HTTP side.
    pub local_ip: IpAddr,
    /// Subscription on the StreamHub — produces PcmFrame entries the
    /// audio thread consumes.
    pub samples_rx: Receiver<PcmFrame>,
    /// Initial volume in [0, 100] — converted to RAOP dB internally.
    pub initial_volume: Option<u32>,
    /// Timeout for the RTSP connect + each request. Callers that have an
    /// AirPlay 2 fallback pass a short value so a device that advertises
    /// `_raop._tcp` but ignores legacy RTSP (e.g. Sonos, which is
    /// AirPlay-2-only) fails fast instead of stalling the full window.
    pub connect_timeout: Duration,
}

/// Live AirPlay session.
pub struct AirPlaySession {
    pub renderer: AirPlayRenderer,
    /// Held under a Mutex so volume commands from arbitrary threads
    /// can serialise into the single RTSP connection.
    rtsp: Arc<Mutex<RtspClient>>,
    /// Signals every background thread to stop (audio sender, timing
    /// responder, sync sender).
    stop_flag: Arc<AtomicBool>,
    /// Handles on the three background threads. Stored as Options so
    /// `stop()` can take them out of the session before joining.
    sender_handle: Option<JoinHandle<()>>,
    timing_handle: Option<JoinHandle<()>>,
    sync_handle: Option<JoinHandle<()>>,
    resend_handle: Option<JoinHandle<()>>,
    /// Hold the audio socket so it isn't closed prematurely; the
    /// sender thread holds a `try_clone` for sending. Once `stop`
    /// runs we drop these in the right order to wake any blocked
    /// background thread.
    _audio_socket: UdpSocket,
}

impl AirPlaySession {
    /// Open the full RAOP session: RTSP handshake, allocate UDP
    /// sockets, RECORD, and spawn the audio thread. Returns once audio
    /// is being delivered.
    pub fn start(cfg: AirPlaySessionConfig) -> Result<Self> {
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

        let mut rtsp = RtspClient::connect(
            cfg.renderer.ip,
            cfg.renderer.port,
            cfg.local_ip,
            cfg.connect_timeout,
        )
        .context("opening RTSP connection")?;

        // Opening handshake — packet-capture verified against iTunes for
        // Windows → Sonos (AirTunes/366 fw): iTunes' FIRST request is
        // POST /auth-setup (0x01 + X25519), then ANNOUNCE — it never sends
        // the classic OPTIONS + Apple-Challenge opener, and this Sonos
        // firmware stalls on it. Receivers advertising MFi (et=4) or an
        // AirPlay-2 side get the iTunes flow; plain legacy receivers keep
        // the traditional OPTIONS + Apple-Challenge opener.
        let mfi_style = cfg.renderer.encryption_types.contains(&4)
            || cfg.renderer.supports_airplay2();
        if mfi_style {
            rtsp.auth_setup().context("RTSP auth-setup (MFi opener)")?;
        } else {
            rtsp.options().context("RTSP OPTIONS")?;
        }

        let cipher = Cipher::pick_for(&cfg.renderer.encryption_types).ok_or_else(|| {
            anyhow::anyhow!(
                "no compatible encryption mode for {} (advertised et={:?})",
                cfg.renderer.friendly_name,
                cfg.renderer.encryption_types,
            )
        })?;
        info!(
            "AirPlay: cipher={} (receiver et={:?})",
            cipher.label(),
            cfg.renderer.encryption_types,
        );
        let cipher = Arc::new(cipher);
        rtsp.announce(&cipher).context("RTSP ANNOUNCE")?;

        let server_ports = rtsp
            .setup(control_port, timing_port)
            .context("RTSP SETUP")?;
        debug!(
            "AirPlay SETUP returned server ports: audio={}, control={}, timing={}",
            server_ports.audio, server_ports.control, server_ports.timing,
        );

        // Random initial RTP state for this session.
        let initial_seq = random_initial_seq();
        let initial_rtptime = random_initial_rtptime();
        let ssrc = random_ssrc();
        rtsp.record(initial_seq, initial_rtptime)
            .context("RTSP RECORD")?;

        if let Some(vol) = cfg.initial_volume {
            // Best-effort — don't fail the session if the volume set
            // bounces (some receivers are picky about timing).
            let db = volume_pct_to_raop_db(vol);
            if let Err(e) = rtsp.set_volume(db) {
                warn!("AirPlay SET_PARAMETER volume failed: {}", e);
            }
        }

        // Spin up the three background threads. The audio sender owns
        // a try_clone of the audio socket; timing + sync own their
        // respective sockets outright (we don't need to keep them in
        // the session struct because the threads block on them).
        let audio_socket_for_thread = audio_socket
            .try_clone()
            .context("try_clone audio socket")?;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let current_rtptime = Arc::new(AtomicU32::new(initial_rtptime));
        let resend = ResendBuffer::new(RESEND_BUFFER_PACKETS);

        // The control socket carries both our outbound sync packets and
        // the receiver's inbound resend requests — clone it so the sync
        // sender and the resend responder can use it concurrently.
        let control_for_resend = control_socket
            .try_clone()
            .context("try_clone control socket")?;

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
        };
        let sender_handle = spawn_audio_sender(sender_cfg)?;

        let timing_handle = spawn_timing_responder(
            timing_socket,
            stop_flag.clone(),
            cfg.renderer.friendly_name.clone(),
        )
        .context("spawning AirPlay timing responder")?;

        let sync_handle = spawn_sync_sender(
            control_socket,
            SocketAddr::new(cfg.renderer.ip, server_ports.control),
            current_rtptime,
            DEFAULT_LATENCY_SAMPLES,
            stop_flag.clone(),
            cfg.renderer.friendly_name.clone(),
        )
        .context("spawning AirPlay sync sender")?;

        let resend_handle = spawn_resend_responder(
            control_for_resend,
            SocketAddr::new(cfg.renderer.ip, server_ports.control),
            resend,
            stop_flag.clone(),
            cfg.renderer.friendly_name.clone(),
        )
        .context("spawning AirPlay resend responder")?;

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
            rtsp: Arc::new(Mutex::new(rtsp)),
            stop_flag,
            sender_handle: Some(sender_handle),
            timing_handle: Some(timing_handle),
            sync_handle: Some(sync_handle),
            resend_handle: Some(resend_handle),
            _audio_socket: audio_socket,
        })
    }

    /// Push a new volume value (0..=100) to the receiver. Idempotent
    /// across repeats; receivers throttle their own UI updates.
    pub fn set_volume_pct(&self, vol: u32) -> Result<()> {
        let db = volume_pct_to_raop_db(vol);
        self.rtsp.lock().unwrap().set_volume(db)
    }

    /// Push a mute state. RAOP doesn't have a separate mute parameter,
    /// just the sentinel -144 dB "off" volume.
    pub fn set_mute(&self, muted: bool) -> Result<()> {
        let db = if muted { -144.0 } else { 0.0 };
        self.rtsp.lock().unwrap().set_volume(db)
    }

    /// Tear down: stop the background threads, TEARDOWN RTSP, close
    /// sockets. Best-effort — receivers will eventually time us out
    /// even if we skip this; it's here for clean shutdown only.
    pub fn stop(mut self) {
        info!(
            "AirPlay: stopping session to {}",
            self.renderer.friendly_name
        );
        self.stop_flag.store(true, Ordering::Release);
        for h in [
            self.sender_handle.take(),
            self.timing_handle.take(),
            self.sync_handle.take(),
            self.resend_handle.take(),
        ]
        .into_iter()
        .flatten()
        {
            let _ = h.join();
        }
        let mut guard = self.rtsp.lock().unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_mapping_anchor_points() {
        assert_eq!(volume_pct_to_raop_db(0), -144.0);
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
}
