//! Central application state and actions.
//!
//! `App` owns everything shared across the audio thread, the optional HTTP
//! server, the egui window, and the system tray. Each consumer holds an
//! `Arc<App>` and calls methods. Atomic-backed knobs are exposed directly
//! for read paths that are on the audio hot path.

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::gena::GenaManager;
use crate::http_server::{SpeakerInfo, StreamHub};
use crate::silence::DEFAULT_QUIESCENT_AFTER_PACKETS;
use crate::ssdp::{DiscoveryState, Renderer};
use crate::volume_sync::VolumeSync;
use crate::{upnp, PRODUCT_NAME, WIRE_SAMPLE_RATE};

/// One running renderer session: the speaker we've handed our stream URL
/// to and its GENA subscription on RenderingControl.
pub struct RendererSession {
    pub renderer: Renderer,
    pub gena: Arc<GenaManager>,
    /// The stream URI we're advertising. Cached for symmetry; same for
    /// every speaker on a given run.
    #[allow(dead_code)]
    pub stream_uri: String,
}

pub type SharedSession = Arc<Mutex<Option<RendererSession>>>;

/// Stable summary of the runtime config that the GUI / tray can render.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub stream_uri: String,
    pub callback_url: String,
    pub advertise_ip: String,
    pub bind: SocketAddr,
    pub initial_buffer_ms: u32,
    pub silence_packets_threshold: u32,
    pub no_silence_injection: bool,
    pub no_discovery: bool,
    pub web_enabled: bool,
}

/// Snapshot of the speaker list + currently-active id. Cheap to compute
/// on demand; the GUI calls this on every repaint.
#[derive(Clone, Debug)]
pub struct SpeakerView {
    pub speakers: Vec<SpeakerInfo>,
    pub active_id: Option<String>,
}

pub struct App {
    /// Resolved-at-startup configuration. Immutable after `new`.
    pub config: AppConfig,

    // ---- Discovery + active session ----
    pub discovery: Option<Arc<DiscoveryState>>,
    pub session: SharedSession,
    /// Remembered speaker id so Disable→Enable can rebind to the same one.
    pub last_speaker_id: Mutex<Option<String>>,

    // ---- Audio data plumbing ----
    pub hub: Arc<StreamHub>,
    pub vsync: Arc<VolumeSync>,

    // ---- Runtime atomics (audio hot path reads these every packet) ----
    /// Driver's KSSTATE_RUN flag mirror — set by the driver-event consumer.
    pub stream_active: Arc<AtomicBool>,
    /// User toggle. False ⇒ no UPnP session, audio goes into the void.
    pub streaming_enabled: Arc<AtomicBool>,
    /// Runtime flag for the web UI / JSON API. Initial value comes from
    /// the `--web` CLI flag, but the user can flip it at runtime from
    /// the GUI. The `/stream.raw` route is always served (the speaker
    /// needs it); only the user-facing routes (`/`, `/api/*`) honour
    /// this flag.
    pub web_ui_enabled: Arc<AtomicBool>,
    /// Pending latency adjustment, in frames. + drops, − duplicates.
    pub drain_frames: Arc<AtomicI64>,
    pub rate_fudge_ppm: Arc<AtomicI32>,
    pub silence_pace_ms: Arc<AtomicU64>,
    pub latency_adjust_step_frames: Arc<AtomicU32>,

    // ---- Stats (best-effort, advisory) ----
    pub packets_published_total: Arc<AtomicU64>,
    pub started_at: Instant,

    // ---- Lifecycle ----
    pub shutdown: Arc<AtomicBool>,
}

impl App {
    pub fn new(config: AppConfig, discovery: Option<Arc<DiscoveryState>>) -> Arc<Self> {
        let web_initial = config.web_enabled;
        Arc::new(Self {
            config,
            discovery,
            session: Arc::new(Mutex::new(None)),
            last_speaker_id: Mutex::new(None),
            hub: StreamHub::new(),
            vsync: Arc::new(VolumeSync::new()),
            stream_active: Arc::new(AtomicBool::new(true)),
            streaming_enabled: Arc::new(AtomicBool::new(true)),
            web_ui_enabled: Arc::new(AtomicBool::new(web_initial)),
            drain_frames: Arc::new(AtomicI64::new(0)),
            rate_fudge_ppm: Arc::new(AtomicI32::new(0)),
            silence_pace_ms: Arc::new(AtomicU64::new(10)),
            latency_adjust_step_frames: Arc::new(AtomicU32::new(4)),
            packets_published_total: Arc::new(AtomicU64::new(0)),
            started_at: Instant::now(),
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    // -------------------------------------------------------------------
    // Speaker view
    // -------------------------------------------------------------------

    /// Snapshot of discovered speakers and which one (if any) is active.
    pub fn speaker_view(&self) -> SpeakerView {
        let active_id = self.session.lock().unwrap().as_ref().map(|s| s.renderer.stable_id());
        let speakers = match self.discovery.as_ref() {
            None => Vec::new(),
            Some(d) => d.renderers().into_iter().map(|r| {
                let id = r.stable_id();
                let active = active_id.as_deref() == Some(id.as_str());
                SpeakerInfo {
                    id,
                    friendly_name: r.friendly_name,
                    ip: r.ip.to_string(),
                    active,
                }
            }).collect(),
        };
        SpeakerView { speakers, active_id }
    }

    pub fn current_renderer(&self) -> Option<Renderer> {
        self.session.lock().unwrap().as_ref().map(|s| s.renderer.clone())
    }

    // -------------------------------------------------------------------
    // Speaker actions
    // -------------------------------------------------------------------

    /// Switch to the speaker with the given stable id. Tears down any
    /// existing session, starts a new one. Remembers the id so Disable→
    /// Enable rebinds to the same speaker.
    pub fn select_speaker(&self, id: &str) -> Result<(), String> {
        let discovery = self.discovery.as_ref()
            .ok_or_else(|| "discovery disabled".to_string())?;
        let Some(new_r) = discovery.find_by_id(id) else {
            return Err(format!("no speaker with id {:?}", id));
        };
        let didl = upnp::didl_lite_metadata(
            &self.config.stream_uri,
            PRODUCT_NAME,
            self.config.initial_buffer_ms,
        );
        let new_session = start_session(
            new_r,
            &self.config.stream_uri,
            &didl,
            &self.config.callback_url,
        ).map_err(|e| format!("{:#}", e))?;
        let mut guard = self.session.lock().unwrap();
        if let Some(old) = guard.take() {
            drop(guard);
            stop_session(&old);
            guard = self.session.lock().unwrap();
        }
        *guard = Some(new_session);
        *self.last_speaker_id.lock().unwrap() = Some(id.to_string());
        self.streaming_enabled.store(true, Ordering::Release);
        Ok(())
    }

    /// Resync — UPnP Stop+Play on the current speaker. Drops Sonos's
    /// accumulated prebuffer; next Play starts fresh with a minimal one.
    pub fn resync(&self) -> Result<(), String> {
        let r = self.current_renderer().ok_or_else(|| "no active speaker".to_string())?;
        info!("resync: UPnP Stop + Play on {}", r.friendly_name);
        if let Err(e) = upnp::stop(&r.av_transport_control_url) {
            debug!("resync stop ignored: {}", e);
        }
        std::thread::sleep(Duration::from_millis(100));
        upnp::play(&r.av_transport_control_url).map_err(|e| format!("play failed: {:#}", e))
    }

    // -------------------------------------------------------------------
    // Enable / disable
    // -------------------------------------------------------------------

    /// User toggle. Disabling tears down the UPnP session (freeing Sonos
    /// for other use); enabling rebinds to the last-used speaker if one
    /// is remembered.
    pub fn set_streaming_enabled(&self, enabled: bool) -> Result<(), String> {
        let was = self.streaming_enabled.swap(enabled, Ordering::AcqRel);
        if was == enabled {
            return Ok(());
        }
        if !enabled {
            let s = self.session.lock().unwrap().take();
            if let Some(s) = s {
                stop_session(&s);
            }
            info!("streaming disabled");
        } else {
            let last = self.last_speaker_id.lock().unwrap().clone();
            match last {
                Some(id) => self.select_speaker(&id)?,
                None => {
                    info!("streaming enabled but no speaker remembered");
                }
            }
        }
        Ok(())
    }

    pub fn is_streaming_enabled(&self) -> bool {
        self.streaming_enabled.load(Ordering::Acquire)
    }

    // -------------------------------------------------------------------
    // Web UI runtime toggle
    // -------------------------------------------------------------------

    pub fn is_web_ui_enabled(&self) -> bool {
        self.web_ui_enabled.load(Ordering::Acquire)
    }

    pub fn set_web_ui_enabled(&self, enabled: bool) {
        let was = self.web_ui_enabled.swap(enabled, Ordering::AcqRel);
        if was != enabled {
            info!("web UI: {}", if enabled { "enabled" } else { "disabled" });
        }
    }

    // -------------------------------------------------------------------
    // Latency adjustment
    // -------------------------------------------------------------------

    /// Schedule a `ms`-millisecond latency adjustment.
    /// `ms > 0` ⇒ drain (drop frames, lower latency).
    /// `ms < 0` ⇒ pad (duplicate frames, higher latency).
    /// Returns the new pending-frames counter.
    pub fn adjust_latency(&self, ms: i32) -> i64 {
        let frames = (ms as i64) * (WIRE_SAMPLE_RATE as i64) / 1000;
        let new = self.drain_frames.fetch_add(frames, Ordering::AcqRel) + frames;
        info!(
            "latency adjust: {:+} ms ({:+} frames) → pending {} frames ({} ms)",
            ms, frames, new, new / 44
        );
        new
    }

    pub fn pending_latency_ms(&self) -> i32 {
        let f = self.drain_frames.load(Ordering::Acquire);
        (f * 1000 / (WIRE_SAMPLE_RATE as i64)) as i32
    }

    // -------------------------------------------------------------------
    // Knob setters
    // -------------------------------------------------------------------

    pub fn set_rate_fudge_ppm(&self, ppm: i32) {
        self.rate_fudge_ppm.store(ppm.clamp(-10_000, 10_000), Ordering::Release);
    }

    pub fn set_silence_pace_ms(&self, ms: u64) {
        self.silence_pace_ms.store(ms.clamp(1, 1000), Ordering::Release);
    }

    pub fn set_latency_adjust_step_frames(&self, frames: u32) {
        self.latency_adjust_step_frames.store(frames.clamp(1, 4_096), Ordering::Release);
    }

    // -------------------------------------------------------------------
    // Stats
    // -------------------------------------------------------------------

    pub fn subscriber_count(&self) -> usize {
        self.hub.subscriber_count()
    }

    pub fn packets_published(&self) -> u64 {
        self.packets_published_total.load(Ordering::Relaxed)
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    // -------------------------------------------------------------------
    // Shutdown
    // -------------------------------------------------------------------

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

// -----------------------------------------------------------------------------
// Session lifecycle helpers
// -----------------------------------------------------------------------------

pub fn start_session(
    renderer: Renderer,
    stream_uri: &str,
    didl: &str,
    callback_url: &str,
) -> Result<RendererSession> {
    info!("targeting speaker: {} ({})", renderer.friendly_name, renderer.ip);
    // Stop first to clear any "Transport Locked" (Sonos 705) carry-over.
    let _ = upnp::stop(&renderer.av_transport_control_url);
    std::thread::sleep(Duration::from_millis(100));
    upnp::set_av_transport_uri(&renderer.av_transport_control_url, stream_uri, didl)
        .context("SetAVTransportURI")?;
    std::thread::sleep(Duration::from_millis(100));
    upnp::play(&renderer.av_transport_control_url).context("Play")?;

    let gena = GenaManager::new(callback_url.to_string());
    match gena.subscribe(&renderer.rendering_control_event_url) {
        Ok(_) => {
            gena.clone().spawn_renewer();
        }
        Err(e) => {
            warn!("GENA subscribe failed (continuing without volume sync): {}", e);
        }
    }

    Ok(RendererSession {
        renderer,
        gena,
        stream_uri: stream_uri.to_string(),
    })
}

pub fn stop_session(session: &RendererSession) {
    session.gena.unsubscribe();
    if let Err(e) = upnp::stop(&session.renderer.av_transport_control_url) {
        debug!("UPnP Stop on {} failed: {}", session.renderer.friendly_name, e);
    }
}

// -----------------------------------------------------------------------------
// IP / bind helpers (moved from main.rs to keep main.rs lean)
// -----------------------------------------------------------------------------

pub fn default_advertise_ip() -> Result<String> {
    use std::net::UdpSocket;
    let s = UdpSocket::bind("0.0.0.0:0").context("binding udp for ip discovery")?;
    if s.connect("8.8.8.8:53").is_ok() {
        if let Ok(addr) = s.local_addr() {
            return Ok(addr.ip().to_string());
        }
    }
    Err(anyhow!("could not determine local IP; pass --advertise-ip"))
}

pub fn pick_ssdp_iface(advertise_ip: Option<&str>, bind: &str) -> Option<std::net::Ipv4Addr> {
    if let Some(s) = advertise_ip {
        if let Ok(IpAddr::V4(v4)) = s.parse() {
            return Some(v4);
        }
    }
    if let Ok(IpAddr::V4(v4)) = bind.parse::<IpAddr>() {
        if !v4.is_unspecified() {
            return Some(v4);
        }
    }
    if let Ok(s) = default_advertise_ip() {
        if let Ok(IpAddr::V4(v4)) = s.parse() {
            return Some(v4);
        }
    }
    None
}

#[allow(dead_code)]
const _: u32 = DEFAULT_QUIESCENT_AFTER_PACKETS;
