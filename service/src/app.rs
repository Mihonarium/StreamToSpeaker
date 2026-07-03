//! Central application state and actions.
//!
//! `App` owns everything shared across the audio thread, the optional HTTP
//! server, the egui window, and the system tray. Each consumer holds an
//! `Arc<App>` and calls methods. Atomic-backed knobs are exposed directly
//! for read paths that are on the audio hot path.

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::airplay::ap2_rtsp::Ap2Rtsp;
use crate::airplay::ap2_session::DEFAULT_AIRPLAY_PORT;
use crate::airplay::hap_pairing::PairingCredentials;
use crate::airplay::{
    AirPlay2Session, AirPlay2SessionConfig, AirPlayDiscoveryState, AirPlayRenderer, AirPlaySession,
    AirPlaySessionConfig, Ap2StartError, Transport,
};
use crate::gena::GenaManager;
use crate::http_server::{SpeakerInfo, StreamHub};
use crate::silence::DEFAULT_QUIESCENT_AFTER_PACKETS;
use crate::ssdp::{DiscoveryState, Renderer};
use crate::user_config::UserConfig;
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

/// Active session — either UPnP (pull-style, speaker fetches our HTTP
/// stream) or AirPlay (push-style, we send RTP/UDP to the speaker).
/// At most one of these is live at a time; switching tears down the
/// old and brings up the new.
pub enum ActiveSession {
    Upnp(RendererSession),
    AirPlay(AirPlaySession),
    AirPlay2(AirPlay2Session),
}

impl ActiveSession {
    pub fn stable_id(&self) -> String {
        match self {
            ActiveSession::Upnp(s) => s.renderer.stable_id(),
            ActiveSession::AirPlay(s) => s.renderer.stable_id(),
            ActiveSession::AirPlay2(s) => s.renderer.stable_id(),
        }
    }

    pub fn friendly_name(&self) -> String {
        match self {
            ActiveSession::Upnp(s) => s.renderer.friendly_name.clone(),
            ActiveSession::AirPlay(s) => s.renderer.friendly_name.clone(),
            ActiveSession::AirPlay2(s) => s.renderer.friendly_name.clone(),
        }
    }

    pub fn ip(&self) -> IpAddr {
        match self {
            ActiveSession::Upnp(s) => s.renderer.ip,
            ActiveSession::AirPlay(s) => s.renderer.ip,
            ActiveSession::AirPlay2(s) => s.renderer.ip,
        }
    }

    /// Tear down — drops GENA subscription / RTSP connection /
    /// background threads. Best-effort; receivers eventually time out
    /// anyway, so failures here aren't fatal.
    pub fn stop(self) {
        match self {
            ActiveSession::Upnp(s) => stop_session(&s),
            ActiveSession::AirPlay(s) => s.stop(),
            ActiveSession::AirPlay2(s) => s.stop(),
        }
    }

    /// Push a volume change to the speaker. UPnP path goes via
    /// SetVolume SOAP; AirPlay path goes via SET_PARAMETER over the
    /// existing RTSP socket.
    pub fn set_volume_pct(&self, pct: u32) -> Result<()> {
        match self {
            ActiveSession::Upnp(s) => {
                upnp::set_volume(&s.renderer.rendering_control_control_url, pct)
            }
            ActiveSession::AirPlay(s) => s.set_volume_pct(pct),
            ActiveSession::AirPlay2(s) => s.set_volume_pct(pct),
        }
    }

    /// A detached metadata sender for the bound speaker, if this session
    /// type supports track metadata (RAOP only today). Cloning it is
    /// cheap and lets the caller send without holding the session lock.
    pub fn metadata_handle(&self) -> Option<crate::airplay::session::MetadataHandle> {
        match self {
            ActiveSession::AirPlay(s) => Some(s.metadata_handle()),
            ActiveSession::AirPlay2(_) | ActiveSession::Upnp(_) => None,
        }
    }

    /// True if the session has died mid-stream (dropped receiver). UPnP
    /// is HTTP-pull (the speaker reconnects on its own), so only the
    /// AirPlay push paths report death here.
    pub fn is_dead(&self) -> bool {
        match self {
            ActiveSession::Upnp(_) => false,
            ActiveSession::AirPlay(s) => s.is_dead(),
            ActiveSession::AirPlay2(s) => s.is_dead(),
        }
    }

    /// Push a mute state to the speaker.
    pub fn set_mute(&self, muted: bool) -> Result<()> {
        match self {
            ActiveSession::Upnp(s) => {
                upnp::set_mute(&s.renderer.rendering_control_control_url, muted)
            }
            ActiveSession::AirPlay(s) => s.set_mute(muted),
            ActiveSession::AirPlay2(s) => s.set_mute(muted),
        }
    }
}

pub type SharedSession = Arc<Mutex<Option<ActiveSession>>>;

/// Transport-agnostic display info for the bound speaker (UPnP, AirPlay 1
/// or AirPlay 2), used by the GUI status banner and the tray label.
pub struct SelectedSpeaker {
    pub friendly_name: String,
    pub ip: IpAddr,
}

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
    /// Local IPv4 interface the SSDP scanner binds to (same iface the
    /// periodic discovery loop uses). Stored so the GUI's Rescan button
    /// can fire a one-shot scan on the same network.
    pub ssdp_iface: Option<Ipv4Addr>,
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
    pub airplay_discovery: Option<Arc<AirPlayDiscoveryState>>,
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

    // ---- Rescan feedback ----
    /// True while a manual SSDP rescan thread is in flight. Cleared
    /// by the rescan thread on exit. Read by the GUI to show a
    /// spinner / "Scanning…" caption.
    pub rescan_in_flight: Arc<AtomicBool>,
    /// Unix-seconds timestamp of the last completed rescan; 0 if
    /// none has run yet. GUI uses this to flash a "Found N speakers"
    /// caption for a few seconds after completion.
    pub last_rescan_finished_unix: Arc<AtomicI64>,
    /// Speaker count at the moment the last rescan completed.
    pub last_rescan_count: Arc<AtomicUsize>,

    /// Persisted user preferences (last picked speaker, onboarding
    /// dismissal). Loaded from disk in `App::new` and saved back via
    /// `App::save_user_config` whenever it changes. Behind a Mutex
    /// because saves happen lazily from any thread (GUI click,
    /// auto-reconnect bg thread, ...).
    pub user_config: Mutex<UserConfig>,

    /// Most recent user-facing error message and when it was recorded.
    /// Surfaced by the GUI as a transient inline banner for ~8 s
    /// (Heuristics F-03 — previously every action that could fail
    /// just `warn!`'d to the log file and the user saw nothing).
    pub last_error: Mutex<Option<(String, Instant)>>,

    /// Friendly name of the speaker a background connect is currently
    /// running for (None when idle). Session bring-up does seconds of
    /// blocking network I/O — pairing, SETUPs, fallbacks — so the GUI
    /// runs it on a worker thread and shows "Connecting…" off this.
    pub connecting: Mutex<Option<String>>,

    /// The one in-flight HomeKit PIN pairing ceremony, if any. Single
    /// source of truth for the whole ceremony lifecycle: reserving the
    /// slot IS the "only one ceremony at a time" lock, the `AwaitingPin`
    /// stage is what the GUI's modal mirrors, and any stage blocks
    /// re-selecting the same device (a second transient pair-setup would
    /// reset the accessory's single HAP pairing session mid-ceremony).
    /// Cleared by the ceremony's RAII guard on every exit path.
    pin_ceremony: Mutex<Option<PinCeremonyState>>,
}

/// A PIN pairing ceremony in flight (see [`App::begin_pin_pairing`]).
struct PinCeremonyState {
    /// Stable id of the receiver being paired.
    id: String,
    /// Friendly name, for the modal + toasts.
    name: String,
    stage: PinStage,
}

enum PinStage {
    /// Connecting + /pair-pin-start — the receiver isn't showing a PIN yet.
    Starting,
    /// The receiver is displaying its PIN; the ceremony thread is parked
    /// on the receiver end of this channel. The GUI modal is (or is about
    /// to be) up; [`App::submit_pin`] sends the PIN (or `None` = cancel).
    AwaitingPin(crossbeam_channel::Sender<Option<String>>),
    /// PIN received — pair-setup M1–M6 is running.
    Pairing,
}

/// Result of one AirPlay bring-up attempt (one entry from
/// `airplay_attempts`). `NeedsPin` is distinguished from a generic failure
/// so `start_airplay` can surface the PIN requirement only after every
/// path — including the fallback legacy RAOP that most Apple TVs also
/// expose — has failed.
enum AttemptError {
    /// The receiver requires one-time PIN pairing (an AP2 470 with no
    /// stored credentials, or stored credentials the receiver rejected).
    NeedsPin,
    /// Any other failure — the message is already user-facing.
    Other(String),
}

/// Failure of a whole [`App::start_airplay`] select: either "this device
/// needs a one-time PIN ceremony" (typed, so the caller decides per its
/// interactivity whether to launch one) or a plain message. Flows through
/// the return path — no side-channel state — so every caller, including
/// the synchronous web-API / tray / launch paths, sees the truth.
enum SelectFailure {
    NeedsPin {
        id: String,
        name: String,
        /// The other attempted paths' failures, pre-joined (may be empty).
        details: String,
    },
    Msg(String),
}

/// RAII cleanup for a PIN pairing ceremony: on any exit (success, error,
/// cancel, timeout, panic, failed thread spawn) it clears the ceremony
/// slot so the modal closes and the user can try again. Constructed right
/// after the slot is reserved and moved into the ceremony thread, so the
/// pre-spawn early-return paths clean up by simply dropping it.
struct CeremonyGuard(Arc<App>);

impl Drop for CeremonyGuard {
    fn drop(&mut self) {
        *self.0.pin_ceremony.lock().unwrap() = None;
    }
}

impl App {
    pub fn new(
        config: AppConfig,
        discovery: Option<Arc<DiscoveryState>>,
        airplay_discovery: Option<Arc<AirPlayDiscoveryState>>,
    ) -> Arc<Self> {
        let web_initial = config.web_enabled;
        let user_config = UserConfig::load();
        // Seed last_speaker_id from disk so that, even before any user
        // action, code that asks "what was the last speaker?" gets the
        // persisted answer.
        let last_speaker_id = Mutex::new(user_config.last_speaker_id.clone());
        Arc::new(Self {
            config,
            discovery,
            airplay_discovery,
            session: Arc::new(Mutex::new(None)),
            last_speaker_id,
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
            rescan_in_flight: Arc::new(AtomicBool::new(false)),
            last_rescan_finished_unix: Arc::new(AtomicI64::new(0)),
            last_rescan_count: Arc::new(AtomicUsize::new(0)),
            user_config: Mutex::new(user_config),
            last_error: Mutex::new(None),
            connecting: Mutex::new(None),
            pin_ceremony: Mutex::new(None),
        })
    }

    /// Record a user-facing error. Logs it AND stashes it for the GUI
    /// to surface as a transient inline banner. Callers should use
    /// short, plain-language messages — the GUI shows them verbatim.
    pub fn record_error(&self, msg: impl Into<String>) {
        let msg = msg.into();
        warn!("{}", msg);
        *self.last_error.lock().unwrap() = Some((msg, Instant::now()));
    }

    /// Returns the current error message if one was recorded within
    /// the last 8 s, else None. The 8-s window is the standard toast
    /// duration; longer would feel sticky.
    pub fn current_error(&self) -> Option<String> {
        let le = self.last_error.lock().unwrap();
        match le.as_ref() {
            Some((msg, when)) if when.elapsed() < Duration::from_secs(8) => Some(msg.clone()),
            _ => None,
        }
    }

    /// Clear the current error (user dismissed the banner).
    pub fn dismiss_error(&self) {
        *self.last_error.lock().unwrap() = None;
    }

    /// Returns true if the user has dismissed the onboarding card.
    /// Persisted across launches via `user_config`.
    pub fn is_onboarding_dismissed(&self) -> bool {
        self.user_config.lock().unwrap().onboarding_dismissed
    }

    /// Mark the onboarding card as dismissed and persist immediately.
    pub fn dismiss_onboarding(&self) {
        let mut uc = self.user_config.lock().unwrap();
        if !uc.onboarding_dismissed {
            uc.onboarding_dismissed = true;
            uc.save();
        }
    }

    /// Undo the onboarding dismissal (Help menu → "Show getting-
    /// started again"). Persists immediately.
    pub fn reset_onboarding(&self) {
        let mut uc = self.user_config.lock().unwrap();
        if uc.onboarding_dismissed {
            uc.onboarding_dismissed = false;
            uc.save();
        }
    }

    /// Returns the persisted "always minimise to tray on window close"
    /// preference (the close-confirm modal's checkbox).
    pub fn is_always_minimise_to_tray(&self) -> bool {
        self.user_config.lock().unwrap().always_minimise_to_tray
    }

    /// Persist the "always minimise to tray" preference.
    pub fn set_always_minimise_to_tray(&self, on: bool) {
        let mut uc = self.user_config.lock().unwrap();
        if uc.always_minimise_to_tray != on {
            uc.always_minimise_to_tray = on;
            uc.save();
        }
    }

    /// Whether to auto-reconnect to the saved speaker on launch.
    pub fn is_auto_reconnect_on_launch(&self) -> bool {
        self.user_config.lock().unwrap().auto_reconnect_on_launch
    }

    /// Whether to auto-reconnect when a live session drops mid-stream.
    pub fn is_auto_reconnect_on_drop(&self) -> bool {
        self.user_config.lock().unwrap().auto_reconnect_on_drop
    }

    /// The stored AirPlay password for a device id, if any.
    pub fn airplay_password(&self, id: &str) -> Option<String> {
        self.user_config.lock().unwrap().airplay_passwords.get(id).cloned()
    }

    /// Store (or clear, when `pw` is empty) the AirPlay password for a
    /// device id and persist it.
    pub fn set_airplay_password(&self, id: &str, pw: &str) {
        let mut uc = self.user_config.lock().unwrap();
        if pw.is_empty() {
            uc.airplay_passwords.remove(id);
        } else {
            uc.airplay_passwords.insert(id.to_string(), pw.to_string());
        }
        uc.save();
    }

    /// Whether a discovered device is password-protected (needs a
    /// password entered before it can connect).
    pub fn is_password_protected(&self, id: &str) -> bool {
        self.airplay_discovery
            .as_ref()
            .and_then(|d| d.find_by_id(id))
            .map(|r| r.password_protected)
            .unwrap_or(false)
    }

    /// Whether to forward Windows "now playing" to the speaker.
    pub fn is_forward_now_playing(&self) -> bool {
        self.user_config.lock().unwrap().forward_now_playing
    }

    /// Persist the now-playing-forwarding preference.
    pub fn set_forward_now_playing(&self, on: bool) {
        let mut uc = self.user_config.lock().unwrap();
        if uc.forward_now_playing != on {
            uc.forward_now_playing = on;
            uc.save();
        }
    }

    /// Persist the drop-reconnect preference.
    pub fn set_auto_reconnect_on_drop(&self, on: bool) {
        let mut uc = self.user_config.lock().unwrap();
        if uc.auto_reconnect_on_drop != on {
            uc.auto_reconnect_on_drop = on;
            uc.save();
        }
    }

    /// Persist the auto-reconnect preference.
    pub fn set_auto_reconnect_on_launch(&self, on: bool) {
        let mut uc = self.user_config.lock().unwrap();
        if uc.auto_reconnect_on_launch != on {
            uc.auto_reconnect_on_launch = on;
            uc.save();
        }
    }

    /// The user's last explicitly-selected speaker, as persisted in
    /// `user_config`. Returns `None` on a fresh install (which is what
    /// makes `main.rs` skip auto-reconnect on the first launch — the
    /// user picks manually that one time).
    pub fn saved_speaker_id(&self) -> Option<String> {
        self.user_config.lock().unwrap().last_speaker_id.clone()
    }

    // -------------------------------------------------------------------
    // Speaker view
    // -------------------------------------------------------------------

    /// Snapshot of discovered speakers and which one (if any) is active.
    /// Merges the UPnP and AirPlay discovery lists; the GUI sees a
    /// single sorted list with one row per speaker regardless of which
    /// protocol it speaks.
    pub fn speaker_view(&self) -> SpeakerView {
        let active_id = self.session.lock().unwrap().as_ref().map(|s| s.stable_id());
        let mut speakers: Vec<SpeakerInfo> = Vec::new();

        // Track the IPs that have a non-AirPlay (UPnP) entry, so we can
        // warn on the AirPlay twin that it'll have higher latency (the
        // AirPlay path buffers ~1–2 s; the UPnP path is lower-latency).
        let mut upnp_ips: std::collections::HashSet<String> = std::collections::HashSet::new();

        if let Some(d) = self.discovery.as_ref() {
            for r in d.renderers() {
                let id = r.stable_id();
                let active = active_id.as_deref() == Some(id.as_str());
                upnp_ips.insert(r.ip.to_string());
                speakers.push(SpeakerInfo {
                    id,
                    friendly_name: r.friendly_name,
                    ip: r.ip.to_string(),
                    active,
                    note: None,
                });
            }
        }
        if let Some(d) = self.airplay_discovery.as_ref() {
            for r in d.renderers() {
                let id = r.stable_id();
                let active = active_id.as_deref() == Some(id.as_str());
                let transport = r.transport();
                let usable = transport.is_some();
                // Same physical speaker also reachable over UPnP → AirPlay
                // will add noticeably more delay; flag it (only when the
                // AirPlay path is actually usable — no point warning about
                // latency on an entry they can't select).
                let has_upnp_twin = usable && upnp_ips.contains(&r.ip.to_string());
                // Annotate the row so the user can tell which protocol a
                // click will use (Sonos advertises both UPnP and AirPlay)
                // and why an unsupported one might fail.
                let pw = if r.password_protected { ", password" } else { "" };
                let name = match transport {
                    Some(Transport::RaopLegacy) if has_upnp_twin => {
                        format!("{} (AirPlay, higher delay{})", r.friendly_name, pw)
                    }
                    Some(Transport::AirPlay2) if has_upnp_twin => {
                        format!("{} (AirPlay 2, higher delay{})", r.friendly_name, pw)
                    }
                    Some(Transport::RaopLegacy) => {
                        format!("{} (AirPlay{})", r.friendly_name, pw)
                    }
                    Some(Transport::AirPlay2) => format!("{} (AirPlay 2{})", r.friendly_name, pw),
                    None if r.password_protected => {
                        format!("{} (AirPlay, password-protected)", r.friendly_name)
                    }
                    None => format!("{} (AirPlay, unsupported)", r.friendly_name),
                };
                let note = has_upnp_twin.then(|| {
                    "This speaker also has a non-AirPlay (UPnP) entry in the list, which has \
                     lower latency. AirPlay buffers about 1–2 seconds of audio, so it will be \
                     more out of sync with video. Prefer the plain entry unless you specifically \
                     need AirPlay."
                        .to_string()
                });
                speakers.push(SpeakerInfo {
                    id,
                    friendly_name: name,
                    ip: r.ip.to_string(),
                    active,
                    note,
                });
            }
        }
        speakers.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        SpeakerView { speakers, active_id }
    }

    /// Returns the active UPnP renderer if the session is UPnP-flavoured.
    /// Returns `None` for AirPlay sessions or no session — callers that
    /// need the SOAP URL only work with UPnP anyway.
    pub fn current_renderer(&self) -> Option<Renderer> {
        match self.session.lock().unwrap().as_ref() {
            Some(ActiveSession::Upnp(s)) => Some(s.renderer.clone()),
            _ => None,
        }
    }

    /// Returns the active AirPlay renderer if the session is AirPlay-
    /// flavoured. Mirror of `current_renderer` for the AirPlay path.
    pub fn current_airplay_renderer(&self) -> Option<AirPlayRenderer> {
        match self.session.lock().unwrap().as_ref() {
            Some(ActiveSession::AirPlay(s)) => Some(s.renderer.clone()),
            _ => None,
        }
    }

    /// Transport-agnostic view of the bound speaker for the UI status
    /// banner and tray — works for UPnP, AirPlay 1 and AirPlay 2 alike.
    pub fn selected_speaker(&self) -> Option<SelectedSpeaker> {
        self.session.lock().unwrap().as_ref().map(|s| SelectedSpeaker {
            friendly_name: s.friendly_name(),
            ip: s.ip(),
        })
    }

    /// True if any speaker (any transport) is currently bound.
    pub fn is_speaker_bound(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    // -------------------------------------------------------------------
    // Speaker actions
    // -------------------------------------------------------------------

    /// Friendly name of an in-flight background connect, if any.
    pub fn connecting_to(&self) -> Option<String> {
        self.connecting.lock().unwrap().clone()
    }

    /// Non-blocking [`select_speaker`]: runs the (seconds-long) session
    /// bring-up on a worker thread so the GUI stays responsive, exposing
    /// progress via [`connecting_to`] and failures via `record_error`.
    /// A second call while one is in flight is refused with a toast.
    ///
    /// This is the *interactive* entry point (GUI speaker click): a
    /// receiver that turns out to need one-time PIN pairing gets the
    /// ceremony launched. Automatic callers (reconnect watchdog, the
    /// ceremony's own post-pair reconnect) use
    /// [`select_speaker_async_opts`] with `interactive = false` so an
    /// unattended retry can never pop a PIN prompt on the user's TV.
    ///
    /// [`select_speaker_async_opts`]: App::select_speaker_async_opts
    pub fn select_speaker_async(self: &Arc<Self>, id: &str) {
        self.select_speaker_async_opts(id, true);
    }

    fn select_speaker_async_opts(self: &Arc<Self>, id: &str, interactive: bool) {
        {
            let mut guard = self.connecting.lock().unwrap();
            if let Some(name) = guard.as_ref() {
                if interactive {
                    self.record_error(format!("Still connecting to {} — give it a moment.", name));
                } else {
                    debug!("select of {} skipped — still connecting to {}", id, name);
                }
                return;
            }
            // Resolve a display name best-effort for the banner.
            let name = self
                .discovery
                .as_ref()
                .and_then(|d| d.find_by_id(id))
                .map(|r| r.friendly_name)
                .or_else(|| {
                    self.airplay_discovery
                        .as_ref()
                        .and_then(|d| d.find_by_id(id))
                        .map(|r| r.friendly_name)
                })
                .unwrap_or_else(|| id.to_string());
            *guard = Some(name);
        }
        let app = self.clone();
        let id = id.to_string();
        std::thread::Builder::new()
            .name("stream-to-speaker-connect".into())
            .spawn(move || {
                let result = app.select_speaker_opts(&id, interactive);
                *app.connecting.lock().unwrap() = None;
                if let Err(e) = result {
                    app.record_error(format!("Couldn't connect to speaker: {}", e));
                }
            })
            .ok();
    }

    /// Background watchdog: when a live session drops mid-stream (speaker
    /// rebooted / Wi-Fi blip / receiver reaped the session), tear it down
    /// and reconnect once — so the UI never sits on a zombie "streaming"
    /// state. Mirrors OwnTone's policy: a single retry, ~5 s after
    /// detection (the spacing also respects the Sonos half-open hold).
    /// Gated on the `auto_reconnect_on_drop` config (default on) and on
    /// streaming being enabled (never fights a user who hit Disable).
    pub fn spawn_reconnect_watchdog(self: &Arc<Self>) {
        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-reconnect-watchdog".into())
            .spawn(move || {
                const RECONNECT_GRACE: Duration = Duration::from_secs(5);
                loop {
                    // Poll ~1 s, sliced so shutdown is prompt.
                    for _ in 0..10 {
                        if app.is_shutting_down() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }

                    if !app.user_config.lock().unwrap().auto_reconnect_on_drop {
                        continue;
                    }
                    if !app.streaming_enabled.load(Ordering::Acquire) {
                        continue;
                    }
                    if app.connecting.lock().unwrap().is_some() {
                        continue;
                    }

                    // Is the bound session dead?
                    let (id, name) = {
                        let guard = app.session.lock().unwrap();
                        match guard.as_ref() {
                            Some(s) if s.is_dead() => (s.stable_id(), s.friendly_name()),
                            _ => continue,
                        }
                    };

                    warn!(
                        "AirPlay session to {} dropped; auto-reconnecting in {} s",
                        name,
                        RECONNECT_GRACE.as_secs()
                    );
                    app.record_error(format!("Lost connection to {} — reconnecting…", name));

                    // Grace before the retry, cancellable by shutdown or a
                    // user Disable.
                    let mut cancelled = false;
                    for _ in 0..(RECONNECT_GRACE.as_millis() / 100) {
                        if app.is_shutting_down() {
                            return;
                        }
                        if !app.streaming_enabled.load(Ordering::Acquire) {
                            cancelled = true;
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if cancelled {
                        continue;
                    }

                    // Confirm it's still the same dead session (a manual
                    // reselect during the grace may have replaced it).
                    let still_dead = app
                        .session
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|s| s.is_dead() && s.stable_id() == id)
                        .unwrap_or(false);
                    if !still_dead {
                        continue;
                    }

                    // Reconnect. `select_speaker` tears the dead session
                    // down first (replacing_same) then brings up a fresh
                    // one; async so this thread never blocks on the
                    // multi-second handshake. One shot per drop: if it
                    // fails the session ends up None and we stop retrying
                    // (no hammering a gone speaker / poking the Sonos hold).
                    // Non-interactive: an unattended retry must never pop
                    // a PIN pairing prompt on the user's TV.
                    app.select_speaker_async_opts(&id, false);
                    while app.connecting.lock().unwrap().is_some() {
                        if app.is_shutting_down() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            })
            .ok();
    }

    /// Background forwarder: when enabled, pushes the OS "now playing"
    /// (title/artist/album) to the bound speaker as it changes, so the
    /// track shows on the speaker's display / app. Off by default; RAOP
    /// only (AP2/UPnP sessions are skipped). Entirely best-effort —
    /// metadata is non-fatal on every receiver, so a failure or an
    /// unsupported OS never touches the audio path.
    pub fn spawn_now_playing_forwarder(self: &Arc<Self>) {
        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-now-playing".into())
            .spawn(move || {
                #[cfg(windows)]
                unsafe {
                    // Best-effort MTA init so the WinRT SMTC calls work on
                    // this thread; harmless if COM is already initialised.
                    use windows::Win32::System::Com::{
                        CoInitializeEx, COINIT_MULTITHREADED,
                    };
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }

                let mut last_sent: Option<crate::now_playing::NowPlaying> = None;
                loop {
                    for _ in 0..20 {
                        if app.is_shutting_down() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }

                    if !app.user_config.lock().unwrap().forward_now_playing {
                        // Reset so re-enabling re-sends the current track.
                        last_sent = None;
                        continue;
                    }

                    // Grab a detached metadata handle (RAOP only) under a
                    // brief session lock, then release it — the network send
                    // below must NOT hold the session mutex (the GUI polls
                    // it every repaint; a slow send would freeze the UI).
                    let handle = app.session.lock().unwrap().as_ref().and_then(|s| s.metadata_handle());
                    let Some(handle) = handle else {
                        last_sent = None;
                        continue;
                    };

                    let np = crate::now_playing::current();
                    if np == last_sent {
                        continue; // unchanged (incl. both None)
                    }

                    if let Some(track) = &np {
                        match handle.send(&track.title, &track.artist, &track.album) {
                            Ok(()) => {
                                debug!("now-playing → speaker: {} — {}", track.artist, track.title);
                            }
                            Err(e) => {
                                // Non-fatal — the receiver may not support
                                // metadata. Record it as attempted anyway so
                                // we don't re-send the SAME track every tick
                                // (which, on a receiver that hangs on it,
                                // could otherwise churn reconnects).
                                debug!("now-playing send failed (non-fatal): {:#}", e);
                            }
                        }
                        // Mark this track attempted regardless of outcome.
                        last_sent = np;
                    } else {
                        // Nothing playing now; remember so we re-send when
                        // it resumes.
                        last_sent = None;
                    }
                }
            })
            .ok();
    }

    /// Switch to the speaker with the given stable id. Tears down any
    /// existing session, starts a new one. Remembers the id so Disable→
    /// Enable rebinds to the same speaker.
    ///
    /// Dispatches based on the id prefix:
    ///   * `"airplay:<mac>"` → AirPlay RAOP session
    ///   * anything else     → UPnP / OpenHome session
    ///
    /// Non-interactive: a receiver that needs one-time PIN pairing yields
    /// an actionable error instead of launching the ceremony (this is the
    /// entry point of the web API / tray toggle / launch reconnect, where
    /// nobody could answer a PIN prompt).
    pub fn select_speaker(self: &Arc<Self>, id: &str) -> Result<(), String> {
        self.select_speaker_opts(id, false)
    }

    fn select_speaker_opts(self: &Arc<Self>, id: &str, interactive: bool) -> Result<(), String> {
        // A PIN ceremony in flight for THIS device owns its pairing state:
        // a concurrent transient pair-setup would reset the accessory's
        // single in-progress HAP pairing session and make the ceremony
        // fail with "wrong PIN" despite a correct one. (Selecting a
        // *different* speaker meanwhile is fine.)
        if let Some((cid, cname)) = self.pin_pairing_device() {
            if cid == id {
                return Err(format!(
                    "Pairing with {} is in progress — enter the PIN shown on its screen.",
                    cname
                ));
            }
        }
        // Re-selecting the receiver that's already bound — e.g. retrying a
        // silent AirPlay session — must tear the old session down FIRST.
        // While we're still streaming to it the receiver is occupied and
        // refuses a fresh pairing/connection (it just times out → 10060).
        // For a *different* receiver we keep the old session until the new
        // one is up, so a failed switch doesn't drop a working stream.
        let replacing_same = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.stable_id() == id)
            .unwrap_or(false);
        if replacing_same {
            if let Some(old) = self.session.lock().unwrap().take() {
                old.stop();
            }
        }

        let new_session = if id.starts_with("airplay:") {
            match self.start_airplay(id) {
                Ok(s) => s,
                Err(SelectFailure::Msg(m)) => return Err(m),
                Err(SelectFailure::NeedsPin { id: pid, name, details }) => {
                    // Keep the other paths' failures visible — the RAOP
                    // fallback's reason would otherwise vanish behind the
                    // pairing message.
                    let suffix = if details.is_empty() {
                        String::new()
                    } else {
                        format!("  (Other paths tried: {})", details)
                    };
                    if interactive {
                        // GUI speaker click: launch the one-time ceremony.
                        // The receiver will display a PIN and the modal
                        // collects it; the toast narrates what's happening.
                        self.begin_pin_pairing(pid, name.clone());
                        return Err(format!(
                            "{} needs one-time pairing — enter the PIN shown on its screen.{}",
                            name, suffix
                        ));
                    }
                    // Automatic/remote caller: nobody could answer a PIN
                    // prompt, so don't make the receiver display one —
                    // tell the user where to do it instead.
                    return Err(format!(
                        "{} needs one-time PIN pairing — open the {} window and click it in \
                         the speaker list to pair.{}",
                        name, PRODUCT_NAME, suffix
                    ));
                }
            }
        } else {
            self.start_upnp(id)?
        };

        let mut guard = self.session.lock().unwrap();
        if let Some(old) = guard.take() {
            drop(guard);
            old.stop();
            guard = self.session.lock().unwrap();
        }
        *guard = Some(new_session);
        // CRITICAL: drop the session-mutex guard before calling any
        // method that re-locks it. std::sync::Mutex is NOT re-entrant
        // — methods further down also call `self.session.lock()`, and
        // on the same thread that's a deadlock. Pull out what we need
        // (we just stored it) and drop the guard immediately so the
        // rest of this function stays lock-free. We grab two things:
        // the UPnP renderer (Some only for a UPnP session — used to
        // prime the volume cache, a SOAP-only path) and the friendly
        // name (works for either protocol — used for the Windows
        // endpoint label).
        let new_upnp_renderer = match guard.as_ref() {
            Some(ActiveSession::Upnp(s)) => Some(s.renderer.clone()),
            _ => None,
        };
        #[cfg(windows)]
        let new_friendly_name = guard.as_ref().map(|s| s.friendly_name());
        drop(guard);

        *self.last_speaker_id.lock().unwrap() = Some(id.to_string());
        self.streaming_enabled.store(true, Ordering::Release);
        // Persist so the next launch can auto-reconnect. We do NOT
        // auto-dismiss the onboarding here — picking a speaker only
        // proves step 2 is done; step 1 (routing Windows audio to
        // Stream To Speaker) is the actual prerequisite for audio
        // to flow, and a user can complete step 2 without realising
        // they still need step 1. The card stays visible until they
        // click "Hide this guide" themselves.
        {
            let mut uc = self.user_config.lock().unwrap();
            if uc.last_speaker_id.as_deref() != Some(id) {
                uc.last_speaker_id = Some(id.to_string());
                uc.save();
            }
        }
        // Update the Windows endpoint name to "Stream To Speaker → {name}"
        // so the user can see in Sound Settings / volume mixer which
        // speaker is the current destination. Best-effort; the call
        // is detached on a thread and failures are debug-logged.
        #[cfg(windows)]
        {
            crate::endpoint_name::update_endpoint_name(new_friendly_name.as_deref());
        }
        // m24: prime the volume cache so the GUI slider shows the
        // speaker's actual level on the next paint, instead of
        // waiting for the first GENA NOTIFY (which can take a few
        // seconds and is silent if the user never adjusts the
        // volume on the speaker side). Detached — the upnp call
        // can block ~100 ms.
        if let Some(r) = new_upnp_renderer {
            let vsync = self.vsync.clone();
            let url = r.rendering_control_control_url.clone();
            std::thread::spawn(move || {
                if let Ok(level) = upnp::get_volume(&url) {
                    vsync.prime_initial_volume(level);
                }
            });
        }
        Ok(())
    }

    fn start_upnp(&self, id: &str) -> Result<ActiveSession, String> {
        let discovery = self
            .discovery
            .as_ref()
            .ok_or_else(|| "UPnP discovery disabled".to_string())?;
        let Some(new_r) = discovery.find_by_id(id) else {
            return Err(format!("no UPnP speaker with id {:?}", id));
        };
        let didl = upnp::didl_lite_metadata(
            &self.config.stream_uri,
            PRODUCT_NAME,
            self.config.initial_buffer_ms,
        );
        let session = start_session(
            new_r,
            &self.config.stream_uri,
            &didl,
            &self.config.callback_url,
        )
        .map_err(|e| format!("{:#}", e))?;
        Ok(ActiveSession::Upnp(session))
    }

    fn start_airplay(&self, id: &str) -> Result<ActiveSession, SelectFailure> {
        let discovery = self
            .airplay_discovery
            .as_ref()
            .ok_or_else(|| SelectFailure::Msg("AirPlay discovery disabled".to_string()))?;
        let Some(renderer) = discovery.find_by_id(id) else {
            return Err(SelectFailure::Msg(format!("no AirPlay speaker with id {:?}", id)));
        };
        debug!(
            "AirPlay select {}: transport={:?} supports_ap2={} features={:?} airplay_port={:?} raop_port={} et={:?}",
            renderer.friendly_name,
            renderer.transport(),
            renderer.supports_airplay2(),
            renderer.features,
            renderer.airplay_port,
            renderer.port,
            renderer.encryption_types,
        );

        // Resolve a local IPv4 to bind UDP sockets to + advertise in
        // SDP. Prefer the explicit `advertise_ip` (which the user can
        // override). It must be reachable by the receiver, so falling
        // back to a 0.0.0.0 here would be wrong.
        let local_ip = self.advertised_local_ip().map_err(SelectFailure::Msg)?;

        // Build the ordered list of paths to try, best first. A device may
        // expose more than one (Sonos advertises a vestigial _raop._tcp it
        // no longer answers, plus a working _airplay._tcp); we try them in
        // order and fall back, so legacy RAOP, AirPlay 2, and HomePod
        // devices all just work.
        let attempts = self.airplay_attempts(&renderer, discovery);
        if attempts.is_empty() {
            return Err(SelectFailure::Msg(format!(
                "{} doesn't advertise an AirPlay path we can use \
                 (codecs={:?}, et={:?}, features={:?}). If it's an Apple TV, make sure \
                 AirPlay is turned on in its settings.",
                renderer.friendly_name,
                renderer.codecs,
                renderer.encryption_types,
                renderer.features,
            )));
        }

        let mut errors: Vec<String> = Vec::new();
        // If an AP2 path wants PIN pairing, surface it (typed) only after
        // every path has failed — most Apple TVs also expose a legacy RAOP
        // path that just works, and a working device must never be
        // interrupted with a pairing prompt.
        let mut pin_target: Option<(String, String)> = None;
        for (transport, r) in attempts {
            let label = match transport {
                Transport::AirPlay2 => "AirPlay 2",
                Transport::RaopLegacy => "AirPlay/RAOP",
            };
            info!("AirPlay: attempting {} to {}", label, r.friendly_name);
            let name = r.friendly_name.clone();
            let id = r.stable_id();
            match self.start_airplay_one(transport, r, local_ip) {
                Ok(session) => return Ok(session),
                Err(AttemptError::NeedsPin) => {
                    warn!("AirPlay {} to {} needs one-time PIN pairing", label, name);
                    pin_target = Some((id, name.clone()));
                }
                Err(AttemptError::Other(e)) => {
                    warn!("AirPlay {} to {} failed: {}", label, name, e);
                    errors.push(format!("{}: {}", label, e));
                }
            }
        }
        // Surface every path's failure — otherwise the real problem (usually
        // the AirPlay 2 attempt) hides behind a fallback RAOP OPTIONS
        // timeout. The joined failures ride along with the NeedsPin outcome
        // too, so the pairing message never masks an independently fixable
        // RAOP error.
        let details = errors.join("  |  ");
        match pin_target {
            Some((id, name)) => Err(SelectFailure::NeedsPin { id, name, details }),
            None => Err(SelectFailure::Msg(details)),
        }
    }

    /// Ordered list of AirPlay paths to try for a selected device, best
    /// first. Receivers that advertise PTP / transient pairing (HomePod,
    /// Sonos) are tried over AirPlay 2 first; legacy receivers over RAOP
    /// first. The other path is always appended as a fallback, and an
    /// AirPlay 2 sibling at the same IP is used when the `_raop` and
    /// `_airplay` records didn't merge into one entry.
    fn airplay_attempts(
        &self,
        renderer: &AirPlayRenderer,
        discovery: &AirPlayDiscoveryState,
    ) -> Vec<(Transport, AirPlayRenderer)> {
        // An AirPlay 2-capable view of this device: the record itself, or a
        // sibling _airplay._tcp entry at the same IP (covers a _raop vs
        // _airplay `deviceid` mismatch that split it into two entries).
        let ap2 = if renderer.supports_airplay2() {
            Some(renderer.clone())
        } else {
            discovery
                .renderers()
                .into_iter()
                .find(|r| r.ip == renderer.ip && r.supports_airplay2())
        };
        let raop = renderer.supports_legacy_raop();

        // Prefer AirPlay 2 only for receivers that genuinely REQUIRE it
        // (HomePods, AP2-only devices). Packet-capture verified: iTunes
        // streams to a current-firmware Sonos via classic RAOP (auth-setup
        // opener, realtime UDP, NTP timing) — advertising PTP/buffered
        // does NOT mean the receiver wants them from third-party senders.
        let ap2_centric = renderer.requires_airplay2()
            || ap2
                .as_ref()
                .map(|r| r.requires_airplay2())
                .unwrap_or(false)
            || (ap2.is_some() && !raop);

        let mut out: Vec<(Transport, AirPlayRenderer)> = Vec::new();
        let push_ap2 = |out: &mut Vec<(Transport, AirPlayRenderer)>| {
            if let Some(r) = &ap2 {
                out.push((Transport::AirPlay2, r.clone()));
            }
        };
        if ap2_centric {
            push_ap2(&mut out);
            if raop {
                out.push((Transport::RaopLegacy, renderer.clone()));
            }
        } else {
            if raop {
                out.push((Transport::RaopLegacy, renderer.clone()));
            }
            push_ap2(&mut out);
        }
        out
    }

    /// Start a single AirPlay path (one entry from [`airplay_attempts`]).
    fn start_airplay_one(
        &self,
        transport: Transport,
        renderer: AirPlayRenderer,
        local_ip: IpAddr,
    ) -> Result<ActiveSession, AttemptError> {
        match transport {
            Transport::RaopLegacy => {
                let samples_rx = self.hub.subscribe();
                let (mfi_encryption, uncompressed_alac, password) = {
                    let uc = self.user_config.lock().unwrap();
                    (
                        uc.airplay_mfi_encryption,
                        uc.airplay_uncompressed_alac,
                        uc.airplay_passwords.get(&renderer.stable_id()).cloned(),
                    )
                };
                let session = AirPlaySession::start(AirPlaySessionConfig {
                    renderer,
                    local_ip,
                    samples_rx,
                    initial_volume: Some(80),
                    // Short — RAOP OPTIONS is a sub-100 ms LAN round-trip, so
                    // a dead/vestigial RAOP service (Sonos) fails fast and we
                    // move on to AirPlay 2.
                    connect_timeout: Duration::from_secs(3),
                    mfi_encryption,
                    uncompressed_alac,
                    password,
                })
                .map_err(|e| AttemptError::Other(format!("{:#}", e)))?;
                Ok(ActiveSession::AirPlay(session))
            }
            Transport::AirPlay2 => {
                let samples_rx = self.hub.subscribe();
                let stable_id = renderer.stable_id();
                let (prefer_realtime, pairing_creds) = {
                    let uc = self.user_config.lock().unwrap();
                    (
                        uc.prefer_realtime_airplay,
                        uc.airplay_pairings.get(&stable_id).cloned(),
                    )
                };
                match AirPlay2Session::start(AirPlay2SessionConfig {
                    renderer,
                    local_ip,
                    samples_rx,
                    initial_volume: Some(80),
                    prefer_realtime,
                    pairing_creds,
                }) {
                    Ok(session) => Ok(ActiveSession::AirPlay2(session)),
                    // 470 with no stored keys: typed, so start_airplay
                    // surfaces the PIN requirement only after the RAOP
                    // fallback has also failed.
                    Err(Ap2StartError::NeedsPin) => Err(AttemptError::NeedsPin),
                    // The receiver ANSWERED pair-verify and rejected the
                    // stored credentials (it forgot the pairing): drop them
                    // and re-pair — OwnTone's "key cleared on verify
                    // failure". Transport failures never take this branch,
                    // so a Wi-Fi blip / receiver reboot can't wipe a valid
                    // pairing.
                    Err(Ap2StartError::VerifyRejected(e)) => {
                        warn!(
                            "stored pairing rejected by the receiver ({:#}) — clearing it",
                            e
                        );
                        self.remove_airplay_pairing(&stable_id);
                        Err(AttemptError::NeedsPin)
                    }
                    Err(Ap2StartError::Other(e)) => Err(AttemptError::Other(format!("{:#}", e))),
                }
            }
        }
    }

    /// Launch a one-time HomeKit PIN pairing ceremony for an AP2 receiver
    /// that refused transient pairing (Apple TV with access control).
    ///
    /// Sequencing mirrors OwnTone's PIN flow: one connection sends
    /// `GET /info` (the canonical opener — some receivers initialise
    /// per-connection state on it) + `/pair-pin-start`, then closes —
    /// OwnTone likewise ends the pin-start session; the PIN stays on the
    /// receiver's screen. When the user has typed the PIN (delivered by
    /// the GUI via [`submit_pin`]; 2-minute window), pair-setup M1–M6 runs
    /// on a **fresh** connection — OwnTone creates a new session at
    /// PIN-entry time too, and a socket parked idle for minutes may have
    /// been reaped by the receiver. On success the credentials are stored
    /// and we reconnect, which pair-verifies and streams.
    ///
    /// Only ever called from an interactive select (GUI speaker click) —
    /// automatic retries must not make a TV display a pairing PIN.
    ///
    /// [`submit_pin`]: App::submit_pin
    fn begin_pin_pairing(self: &Arc<Self>, id: String, name: String) {
        // Reserve the single ceremony slot; holding it also blocks
        // re-selects of this device (see select_speaker_opts).
        {
            let mut slot = self.pin_ceremony.lock().unwrap();
            if let Some(existing) = slot.as_ref() {
                debug!(
                    "PIN pairing already in progress for {}; ignoring request for {}",
                    existing.name, name
                );
                return;
            }
            *slot = Some(PinCeremonyState {
                id: id.clone(),
                name: name.clone(),
                stage: PinStage::Starting,
            });
        }
        // From here on the guard owns the slot: every exit — early return,
        // ceremony-thread end, failed spawn (the unexecuted closure drops
        // it), panic — clears it.
        let guard = CeremonyGuard(self.clone());

        let Some(renderer) = self
            .airplay_discovery
            .as_ref()
            .and_then(|d| d.find_by_id(&id))
        else {
            self.record_error(format!("Can't pair {} — it's no longer discovered.", name));
            return;
        };
        let local_ip = match self.advertised_local_ip() {
            Ok(ip) => ip,
            Err(e) => {
                self.record_error(format!("Can't pair {}: {}", name, e));
                return;
            }
        };

        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-ap2-pair".into())
            .spawn(move || {
                let guard = guard;
                let port = renderer.airplay_port.unwrap_or(DEFAULT_AIRPLAY_PORT);

                // Connection 1: ask the receiver to display its PIN.
                let pin_shown = (|| -> Result<()> {
                    let mut rtsp =
                        Ap2Rtsp::connect(renderer.ip, port, local_ip, Duration::from_secs(5))?;
                    if let Err(e) = rtsp.get_info() {
                        debug!("pairing GET /info failed (continuing): {:#}", e);
                    }
                    rtsp.pin_start()
                })();
                if let Err(e) = pin_shown {
                    app.record_error(format!("Couldn't start pairing on {}: {:#}", name, e));
                    return;
                }

                // Publish the awaiting-PIN stage (opens the GUI modal) and
                // park until the user answers or the window expires. 2 min
                // matches the window Apple TVs give for on-screen PIN entry.
                let (tx, rx) = crossbeam_channel::bounded::<Option<String>>(1);
                {
                    let mut slot = app.pin_ceremony.lock().unwrap();
                    match slot.as_mut() {
                        Some(state) => state.stage = PinStage::AwaitingPin(tx),
                        None => return, // shutdown raced us
                    }
                }
                let pin = match rx.recv_timeout(Duration::from_secs(120)) {
                    Ok(Some(pin)) => pin,
                    Ok(None) => {
                        info!("PIN pairing for {} cancelled by user", name);
                        return;
                    }
                    Err(_) => {
                        app.record_error(format!(
                            "PIN entry for {} timed out — select it again to retry.",
                            name
                        ));
                        return;
                    }
                };

                // Stage is Pairing now (submit_pin flipped it). Claim the
                // "Connecting…" banner while pair-setup runs so the GUI
                // shows progress and competing selects are refused
                // (best-effort — if another connect owns it, proceed
                // without).
                let claimed_banner = {
                    let mut c = app.connecting.lock().unwrap();
                    if c.is_none() {
                        *c = Some(format!("{} (pairing)", name));
                        true
                    } else {
                        false
                    }
                };

                // Connection 2 — fresh, at PIN-entry time. The pairing
                // identity is per-install and persistent: re-pairing with
                // the same controller id REPLACES the accessory's stored
                // pairing record instead of leaking one of its finite
                // pairing slots per ceremony.
                let (ctrl_id, ctrl_seed_hex) = app.airplay_controller_identity();
                let setup = (|| -> Result<PairingCredentials> {
                    let mut rtsp =
                        Ap2Rtsp::connect(renderer.ip, port, local_ip, Duration::from_secs(5))?;
                    if let Err(e) = rtsp.get_info() {
                        debug!("pairing GET /info failed (continuing): {:#}", e);
                    }
                    rtsp.pair_setup_pin(pin.trim(), &ctrl_id, &ctrl_seed_hex)
                })();
                if claimed_banner {
                    *app.connecting.lock().unwrap() = None;
                }

                match setup {
                    Ok(creds) => {
                        app.store_airplay_pairing(&id, creds);
                        info!("AirPlay 2: paired {} — connecting", name);
                        // Release the ceremony slot BEFORE reconnecting —
                        // the select guard refuses selects for the pairing
                        // device while the slot is held.
                        drop(guard);
                        // Wait out any in-flight connect so the reconnect
                        // isn't swallowed by the connecting guard.
                        for _ in 0..300 {
                            if app.is_shutting_down() {
                                return;
                            }
                            if app.connecting.lock().unwrap().is_none() {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        app.select_speaker_async_opts(&id, false);
                    }
                    Err(e) => {
                        app.record_error(format!(
                            "Pairing {} failed: {:#} (check the PIN and try again)",
                            name, e
                        ));
                    }
                }
            })
            .ok();
    }

    /// The persistent per-install HomeKit controller identity (pairing id
    /// + Ed25519 seed, hex), generated once and reused for every PIN
    /// ceremony — pairing again with the same controller id replaces the
    /// accessory's stored record instead of consuming another of its
    /// finite pairing slots.
    fn airplay_controller_identity(&self) -> (String, String) {
        let mut uc = self.user_config.lock().unwrap();
        if let (Some(id), Some(seed)) = (
            uc.airplay_controller_id.clone(),
            uc.airplay_controller_seed_hex.clone(),
        ) {
            return (id, seed);
        }
        let (id, seed) = crate::airplay::hap_pairing::generate_controller_identity();
        uc.airplay_controller_id = Some(id.clone());
        uc.airplay_controller_seed_hex = Some(seed.clone());
        uc.save();
        (id, seed)
    }

    /// Persist HomeKit pairing credentials for a device id.
    pub fn store_airplay_pairing(&self, id: &str, creds: PairingCredentials) {
        let mut uc = self.user_config.lock().unwrap();
        uc.airplay_pairings.insert(id.to_string(), creds);
        uc.save();
    }

    /// Remove stored HomeKit pairing credentials for a device id (the
    /// receiver rejected them — it forgot the pairing).
    fn remove_airplay_pairing(&self, id: &str) {
        let mut uc = self.user_config.lock().unwrap();
        if uc.airplay_pairings.remove(id).is_some() {
            uc.save();
        }
    }

    /// The advertised local IP parsed, with a user-facing error.
    fn advertised_local_ip(&self) -> Result<IpAddr, String> {
        self.config
            .advertise_ip
            .parse()
            .map_err(|e| format!("bad advertise_ip {:?}: {}", self.config.advertise_ip, e))
    }

    /// The device a PIN ceremony is currently pairing (any stage), if one
    /// is in flight.
    fn pin_pairing_device(&self) -> Option<(String, String)> {
        self.pin_ceremony
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| (s.id.clone(), s.name.clone()))
    }

    /// GUI: the (id, name) of a PIN pairing ceremony awaiting the user's
    /// PIN, or None. The GUI shows a modal while this is Some (only the
    /// AwaitingPin stage — while the receiver hasn't shown a PIN yet, or
    /// pair-setup is already running, there is nothing to type).
    pub fn pending_pin_pairing(&self) -> Option<(String, String)> {
        let guard = self.pin_ceremony.lock().unwrap();
        match guard.as_ref() {
            Some(s) if matches!(s.stage, PinStage::AwaitingPin(_)) => {
                Some((s.id.clone(), s.name.clone()))
            }
            _ => None,
        }
    }

    /// GUI: deliver the entered PIN (`Some`) or cancel the ceremony
    /// (`None`). Wakes the parked ceremony thread. Returns false when no
    /// ceremony was awaiting a PIN (it expired or was never started) —
    /// the caller should tell the user to re-select the speaker, since a
    /// PIN typed into a stale modal would otherwise vanish silently.
    pub fn submit_pin(&self, pin: Option<String>) -> bool {
        let mut guard = self.pin_ceremony.lock().unwrap();
        let Some(state) = guard.as_mut() else {
            return false;
        };
        if !matches!(state.stage, PinStage::AwaitingPin(_)) {
            return false;
        }
        let PinStage::AwaitingPin(tx) = std::mem::replace(&mut state.stage, PinStage::Pairing)
        else {
            unreachable!("stage checked above");
        };
        tx.send(pin).is_ok()
    }

    /// Current speaker-side volume (0-100) if known. None on a fresh
    /// session before the first prime_initial_volume / GENA NOTIFY.
    pub fn current_volume(&self) -> Option<u32> {
        self.vsync.current_level()
    }

    /// Push a user-set volume to the bound speaker. No-op if no
    /// speaker is bound. Runs the network call on a detached thread so
    /// the caller (GUI slider) doesn't block. Routes through the
    /// session enum's `set_volume_pct`, so it works for both the UPnP
    /// (SetVolume SOAP) and AirPlay (SET_PARAMETER) paths.
    pub fn set_speaker_volume(&self, level: u32) {
        let level = level.min(100);
        // Update the cache immediately so the slider doesn't snap
        // back to the old value while the network call is in flight.
        self.vsync.prime_initial_volume(level);
        let session = self.session.clone();
        std::thread::spawn(move || {
            let guard = session.lock().unwrap();
            if let Some(s) = guard.as_ref() {
                if let Err(e) = s.set_volume_pct(level) {
                    log::warn!("set speaker volume to {} failed: {:#}", level, e);
                }
            }
        });
    }

    /// Disconnect from the current speaker (if any), clear the
    /// persisted `last_speaker_id`, and reset the onboarding-dismissed
    /// flag so the next launch is a true "first launch" again. Used
    /// by the GUI's Forget-speaker button (audit Heuristics F-07 —
    /// without this the only way to clear the saved speaker was to
    /// edit `%APPDATA%\StreamToSpeaker\config.json` by hand).
    pub fn forget_saved_speaker(&self) {
        let s = self.session.lock().unwrap().take();
        if let Some(s) = s {
            s.stop();
        }
        self.streaming_enabled.store(false, Ordering::Release);
        *self.last_speaker_id.lock().unwrap() = None;
        let mut uc = self.user_config.lock().unwrap();
        uc.last_speaker_id = None;
        uc.onboarding_dismissed = false;
        uc.save();
        // Revert the Windows endpoint name back to "Stream To Speaker"
        // since we're no longer streaming to anything specific.
        #[cfg(windows)]
        crate::endpoint_name::update_endpoint_name(None);
    }

    /// Fire a one-shot SSDP M-SEARCH on the same interface the periodic
    /// loop uses. Runs on a detached thread because `discover_once`
    /// blocks for ~3 s waiting for responses; the GUI button doesn't
    /// want to freeze. Results land in `DiscoveryState::replace` and
    /// are picked up by the next `speaker_view()` call.
    pub fn trigger_rescan(&self) {
        let Some(discovery) = self.discovery.clone() else {
            warn!("trigger_rescan: discovery disabled");
            return;
        };
        // Skip if a scan is already running — the user can hammer the
        // button but we don't need two scans racing.
        if self.rescan_in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let iface = self.config.ssdp_iface;
        let in_flight = self.rescan_in_flight.clone();
        let last_finished = self.last_rescan_finished_unix.clone();
        let last_count = self.last_rescan_count.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-rescan".to_string())
            .spawn(move || {
                let count = match crate::ssdp::discover_once(Duration::from_secs(3), iface) {
                    Ok(found) => {
                        let n = found.len();
                        info!("manual rescan: {} renderer(s) found", n);
                        discovery.replace(found);
                        n
                    }
                    Err(e) => {
                        warn!("manual rescan failed: {}", e);
                        0
                    }
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                last_count.store(count, Ordering::Release);
                last_finished.store(now, Ordering::Release);
                in_flight.store(false, Ordering::Release);
            })
            .ok();
    }

    /// Resync — drop the speaker's accumulated prebuffer.
    ///
    /// UPnP: Stop + Play, Sonos picks a fresh prebuffer level.
    /// AirPlay: tear down and rebuild the session, which is the only
    /// reliable way to flush RAOP's own jitter buffer (FLUSH is more
    /// surgical but receivers differ on what it actually clears).
    pub fn resync(self: &Arc<Self>) -> Result<(), String> {
        // Look at the active session under the lock briefly to figure
        // out the dispatch, but don't hold the lock during the slow
        // network calls.
        enum Kind {
            Upnp(Renderer),
            AirPlay(String),
        }
        let kind = {
            let guard = self.session.lock().unwrap();
            match guard.as_ref() {
                Some(ActiveSession::Upnp(s)) => Kind::Upnp(s.renderer.clone()),
                Some(ActiveSession::AirPlay(s)) => {
                    Kind::AirPlay(s.renderer.stable_id())
                }
                Some(ActiveSession::AirPlay2(s)) => {
                    // Same recovery as AirPlay 1: tear down + reconnect.
                    Kind::AirPlay(s.renderer.stable_id())
                }
                None => return Err("no active speaker".to_string()),
            }
        };
        match kind {
            Kind::Upnp(r) => {
                info!("resync: UPnP Stop + Play on {}", r.friendly_name);
                if let Err(e) = upnp::stop(&r.av_transport_control_url) {
                    debug!("resync stop ignored: {}", e);
                }
                std::thread::sleep(Duration::from_millis(100));
                upnp::play(&r.av_transport_control_url)
                    .map_err(|e| format!("play failed: {:#}", e))
            }
            Kind::AirPlay(id) => {
                info!("resync: AirPlay tear-down + reconnect for {}", id);
                self.select_speaker(&id)
            }
        }
    }

    // -------------------------------------------------------------------
    // Enable / disable
    // -------------------------------------------------------------------

    /// User toggle. Disabling tears down the UPnP session (freeing Sonos
    /// for other use); enabling rebinds to the last-used speaker if one
    /// is remembered.
    pub fn set_streaming_enabled(self: &Arc<Self>, enabled: bool) -> Result<(), String> {
        let was = self.streaming_enabled.swap(enabled, Ordering::AcqRel);
        if was == enabled {
            return Ok(());
        }
        if !enabled {
            let s = self.session.lock().unwrap().take();
            if let Some(s) = s {
                s.stop();
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
