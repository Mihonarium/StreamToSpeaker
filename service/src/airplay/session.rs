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

use crate::airplay::crypto::{Cipher, MfiKey};
use crate::airplay::discovery::AirPlayRenderer;
use crate::airplay::rtp::{
    bind_udp, random_initial_rtptime, random_initial_seq, random_ssrc, spawn_audio_sender,
    RtpSenderConfig,
};
use crate::airplay::rtsp::{RtspClient, ServerPorts};
use crate::airplay::timing::{
    sleep_unless_stopped, spawn_resend_responder, spawn_sync_sender, spawn_timing_responder,
    ResendBuffer,
};
use crate::http_server::PcmFrame;

/// Recently-sent packets retained for retransmit (~4 s at 44.1 kHz).
const RESEND_BUFFER_PACKETS: usize = 512;

/// Playback latency in samples for the sync-packet anchor. 88200 = 2 s at
/// 44.1 kHz — packet-capture verified: iTunes→Sonos sync packets carry
/// exactly `next_rtptime − 88200` (and node_airtunes2 hardcodes
/// `2 * sampling_rate`).
const DEFAULT_LATENCY_SAMPLES: u32 = 88200;

/// The fixed sample floor RAOP senders add to a receiver-advertised
/// `Audio-Latency` (libraop's `RAOP_LATENCY_MIN`). 88200 = the receiver's
/// 77175 + this 11025.
const RAOP_LATENCY_FLOOR: u32 = 11025;

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
    /// The background threads. All of them watch `stop_flag` with
    /// bounded wakeups, so `stop()` just sets the flag and joins.
    threads: Vec<JoinHandle<()>>,
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
        let attempt = |try_mfi: bool| -> Result<(RtspClient, Cipher, ServerPorts, u32)> {
            let mut rtsp = RtspClient::connect(
                cfg.renderer.ip,
                cfg.renderer.port,
                cfg.local_ip,
                cfg.connect_timeout,
            )
            .context("opening RTSP connection")?;

            let cipher = if try_mfi {
                // iTunes encrypts audio to et=4 receivers with a key
                // wrapped under the auth-setup ECDH secret (mfiaeskey).
                let shared = rtsp
                    .auth_setup()
                    .context("RTSP auth-setup (MFi opener)")?;
                Cipher::Mfi(MfiKey::derive(&shared))
            } else {
                if mfi_style {
                    // Throwaway unlock — same as OwnTone/libraop: complete
                    // the auth-setup exchange, ignore the secret, stream
                    // plaintext. Field-proven recipe for Sonos-class
                    // receivers.
                    let _ = rtsp.auth_setup().context("RTSP auth-setup")?;
                } else {
                    rtsp.options().context("RTSP OPTIONS")?;
                }
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

            rtsp.announce(&cipher).context("RTSP ANNOUNCE")?;
            let ports = rtsp.setup(control_port, timing_port).context("RTSP SETUP")?;
            let advertised_latency = rtsp
                .record(initial_seq, initial_rtptime)
                .context("RTSP RECORD")?;
            // Honor a receiver that asks for a deeper buffer (big-DSP AVRs
            // report a large Audio-Latency); the reference senders add the
            // 11025-sample floor. Default (no header, e.g. Sonos) keeps the
            // proven 88200 anchor exactly.
            let latency = advertised_latency
                .map(|l| l.saturating_add(RAOP_LATENCY_FLOOR))
                .unwrap_or(0)
                .max(DEFAULT_LATENCY_SAMPLES);
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
            // bounces (some receivers are picky about timing).
            let db = volume_pct_to_raop_db(vol);
            if let Err(e) = rtsp.set_volume(db) {
                warn!("AirPlay SET_PARAMETER volume failed: {}", e);
            }
        }

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
            current_rtptime,
            resend: resend.clone(),
            uncompressed_alac: cfg.uncompressed_alac,
        };
        guard.adopt(spawn_audio_sender(sender_cfg)?);

        guard.adopt(
            spawn_resend_responder(
                control_for_resend,
                receiver_control_addr,
                resend,
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
        // while our UI still says "streaming". Failures don't kill the
        // thread (a Wi-Fi blip must not silently disable keepalives);
        // it backs off and keeps probing so recovery is automatic.
        let keepalive_handle = {
            let rtsp = rtsp.clone();
            let stop_flag = stop_flag.clone();
            let name = cfg.renderer.friendly_name.clone();
            std::thread::Builder::new()
                .name(format!("stream-to-speaker-airplay-keepalive:{}", name))
                .spawn(move || {
                    let mut healthy = true;
                    loop {
                        let interval = if healthy {
                            Duration::from_secs(2)
                        } else {
                            // Back off while the receiver is unresponsive so a
                            // blocked request (3 s read timeout) doesn't hog the
                            // RTSP mutex from volume changes and stop().
                            Duration::from_secs(10)
                        };
                        if !sleep_unless_stopped(&stop_flag, interval) {
                            return;
                        }
                        // try_lock: skip the round rather than queue behind an
                        // in-flight volume change — the next round covers it.
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
                                if !healthy {
                                    info!("AirPlay keepalive to {} recovered", name);
                                }
                                healthy = true;
                            }
                            Err(e) => {
                                if healthy {
                                    warn!(
                                        "AirPlay keepalive to {} failed ({}); receiver may \
                                         have dropped the session — retrying every 10 s",
                                        name, e
                                    );
                                }
                                healthy = false;
                            }
                        }
                    }
                })
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
            threads: guard.into_threads(),
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
        for h in self.threads.drain(..) {
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
