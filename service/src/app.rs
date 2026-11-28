//! Central application state and actions.
//!
//! `App` owns everything shared across the audio thread, the optional HTTP
//! server, the egui window, and the system tray. Each consumer holds an
//! `Arc<App>` and calls methods. Atomic-backed knobs are exposed directly
//! for read paths that are on the audio hot path.

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::update_check::{self, ReleaseInfo};

use crate::airplay::ap2_rtsp::Ap2Rtsp;
use crate::airplay::ap2_session::DEFAULT_AIRPLAY_PORT;
use crate::airplay::hap_pairing::{PairingCredentials, StoredPairing};
use crate::airplay::{
    AirPlay2Session, AirPlay2SessionConfig, AirPlayDiscoveryState, AirPlayRenderer, AirPlaySession,
    AirPlaySessionConfig, Ap2StartError, Transport,
};
use crate::discovery_net::{self, MdnsController, NetAdapter, Resolved, SavedAdapter};
use crate::diagnostics::{Fault, Snapshot, WindowsAudio};
use crate::equalizer::{EqControl, EqReport, EqSettings};
use crate::gena::GenaManager;
use crate::http_server::{SpeakerInfo, StreamHub};
use crate::manual_speakers::{self, ManualKind, ManualSpeaker};
use crate::silence::DEFAULT_QUIESCENT_AFTER_PACKETS;
use crate::ssdp::{DiscoveryState, Renderer, SearchScope};
use crate::stream_profile::{self, Sinks, StreamProfile};
use crate::stream_gate::{canonical_ip, is_local_address, Grant, Refusal, StreamGate};
use crate::user_config::{SpeakerSettings, UserConfig};
#[cfg(test)]
use crate::user_config::AutoConnect;
use crate::volume_sync::{sonos_to_millibels, PendingCommands, SpeakerCommand, VolumeSync};
use crate::web_access::{self, WebAccess, WebSecret};
use crate::{upnp, PRODUCT_NAME, WIRE_SAMPLE_RATE};

/// One running renderer session: the speaker we've handed our stream URL
/// to and its GENA subscription on RenderingControl.
pub struct RendererSession {
    pub renderer: Renderer,
    pub gena: Arc<GenaManager>,
    /// The stream URI this speaker was given (it carries this PC's
    /// address on the speaker's network).
    #[allow(dead_code)]
    pub stream_uri: String,
    /// The stream gate's grant for this speaker's addresses (what privacy
    /// mode admits). Lives exactly as long as the session: dropping the
    /// session revokes them.
    pub stream_grant: Option<Grant>,
    /// The subscription to the speaker's transport events (what it plays,
    /// whether it re-buffers), when it offers them.
    pub transport: Option<Arc<GenaManager>>,
    /// A Sonos room's subscription to its system's topology (which rooms
    /// play together).
    pub topology: Option<Arc<GenaManager>>,
    /// Id the session reports instead of the renderer's own (a speaker
    /// added by address keeps its manual id, so the list highlights the
    /// row the user clicked and a relaunch reconnects through it).
    pub id_override: Option<String>,
    /// How the stream at `stream_uri` is served (see `stream_profile`).
    pub profile: StreamProfile,
    /// When the renderer was given `stream_uri`.
    pub stream_given_at: Instant,
    /// When it accepted Play.
    pub played_at: Instant,
}

/// Active session — either UPnP (pull-style, speaker fetches our HTTP
/// stream) or AirPlay (push-style, we send RTP/UDP to the speaker).
/// At most one of these is live at a time; switching tears down the
/// old and brings up the new.
pub enum ActiveSession {
    Upnp(RendererSession),
    AirPlay(AirPlaySession),
    AirPlay2(AirPlay2Session),
    /// Sendspin player (we push timestamped audio, like AirPlay).
    Sendspin(crate::sendspin::server::PlayerSession),
    /// Music Assistant as the output: its Sendspin input gets our audio.
    MusicAssistant(crate::sendspin_app::MaOutputSession),
}

impl ActiveSession {
    pub fn stable_id(&self) -> String {
        match self {
            ActiveSession::Upnp(s) => s
                .id_override
                .clone()
                .unwrap_or_else(|| s.renderer.stable_id()),
            ActiveSession::AirPlay(s) => s.renderer.stable_id(),
            ActiveSession::AirPlay2(s) => s.renderer.stable_id(),
            ActiveSession::Sendspin(s) => s.renderer.stable_id(),
            ActiveSession::MusicAssistant(_) => crate::sendspin_app::MA_OUTPUT_ID.to_string(),
        }
    }

    /// Does this session drive the speaker selected as `id` (resolved to
    /// `target` for UPnP ids)?
    pub fn is_device(&self, id: &str, target: Option<&Renderer>) -> bool {
        if self.stable_id() == id {
            return true;
        }
        match (self, target) {
            (ActiveSession::Upnp(s), Some(t)) => s.renderer.same_device_as(t),
            _ => false,
        }
    }

    /// Is this a session to a receiver that announces another speaker's
    /// identity (see `RaopDetails::newcomer`)? Asked of the record, so
    /// pairs count their members.
    pub fn is_newcomer(&self) -> bool {
        match self {
            ActiveSession::AirPlay(s) => s.renderer.is_newcomer(),
            ActiveSession::AirPlay2(s) => s.renderer.is_newcomer(),
            _ => false,
        }
    }

    pub fn friendly_name(&self) -> String {
        match self {
            // display_name shows "Living Room + Kitchen" when streaming
            // to a Sonos group (identical to friendly_name otherwise).
            ActiveSession::Upnp(s) => s.renderer.display_name(),
            ActiveSession::AirPlay(s) => s.renderer.friendly_name.clone(),
            ActiveSession::AirPlay2(s) => s.renderer.friendly_name.clone(),
            ActiveSession::Sendspin(s) => s.renderer.friendly_name.clone(),
            ActiveSession::MusicAssistant(s) => s.name.clone(),
        }
    }

    pub fn ip(&self) -> IpAddr {
        match self {
            ActiveSession::Upnp(s) => s.renderer.ip,
            ActiveSession::AirPlay(s) => s.renderer.ip,
            ActiveSession::AirPlay2(s) => s.renderer.ip,
            ActiveSession::Sendspin(s) => s.renderer.ip,
            ActiveSession::MusicAssistant(s) => s.ip(),
        }
    }

    /// Make the speaker silent, for a speaker that still plays this PC's
    /// stream, before this PC plays the sound itself: UPnP `Stop`;
    /// AirPlay 1 `FLUSH`, then `TEARDOWN`; AirPlay 2 its stop as ever
    /// (`FLUSHBUFFERED` for a buffered stream, then `TEARDOWN`). Whether
    /// the speaker took it (an AirPlay 2 receiver's stop has no answer to
    /// read and counts as taken; so do Sendspin's and Music Assistant's).
    pub fn silence(self) -> bool {
        match self {
            ActiveSession::Upnp(s) => {
                unsubscribe_all(&s);
                match upnp::stop(&s.renderer.av_transport_control_url) {
                    Ok(()) => true,
                    Err(e) => {
                        debug!("UPnP Stop on {} failed: {}", s.renderer.friendly_name, e);
                        false
                    }
                }
            }
            ActiveSession::AirPlay(s) => s.silence(),
            other => {
                other.stop();
                true
            }
        }
    }

    /// Tear down — drops GENA subscription / RTSP connection /
    /// background threads. Failures aren't fatal, but the AirPlay 1
    /// TEARDOWN matters: a receiver that isn't told keeps the session for
    /// a while and refuses the next connect.
    pub fn stop(self) {
        match self {
            ActiveSession::Upnp(s) => stop_session(&s),
            ActiveSession::AirPlay(s) => s.stop(),
            ActiveSession::AirPlay2(s) => s.stop(),
            ActiveSession::Sendspin(s) => s.stop(),
            ActiveSession::MusicAssistant(s) => s.stop(),
        }
    }

    /// Whether [`Self::volume_control`] has one: false for Music
    /// Assistant, which keeps its own volume per speaker.
    pub fn has_volume_control(&self) -> bool {
        !matches!(self, ActiveSession::MusicAssistant(_))
    }

    /// The speaker's volume and mute control, detached from the session
    /// (see [`VolumeControl`]). UPnP goes via SetVolume SOAP
    /// (SetGroupVolume when streaming to a Sonos group, so the whole
    /// group scales like its Sonos-app slider); AirPlay via SET_PARAMETER
    /// over the existing RTSP socket. `None` for Music Assistant, which
    /// keeps its own volume per speaker.
    pub fn volume_control(&self) -> Option<VolumeControl> {
        match self {
            ActiveSession::Upnp(s) => Some(VolumeControl::upnp(&s.renderer)),
            ActiveSession::AirPlay(s) => Some(VolumeControl::AirPlay(s.volume_control())),
            ActiveSession::AirPlay2(s) => Some(VolumeControl::AirPlay2(s.volume_control())),
            ActiveSession::Sendspin(s) => Some(VolumeControl::Sendspin(s.volume_control())),
            ActiveSession::MusicAssistant(_) => None,
        }
    }

    /// The speaker's level as its session starts, when known without
    /// asking it. AirPlay starts at the level the app sets. Sendspin uses
    /// the player's level when reported, or the app's level for a player
    /// with fixed volume. A UPnP speaker has to be asked separately.
    pub fn level_on_connect(&self) -> Option<u32> {
        match self {
            ActiveSession::AirPlay(s) => Some(s.volume_pct()),
            ActiveSession::AirPlay2(s) => Some(s.volume_pct()),
            ActiveSession::Sendspin(s) => s.volume(),
            ActiveSession::Upnp(_) | ActiveSession::MusicAssistant(_) => None,
        }
    }

    /// A detached metadata sender for the bound speaker, if this session
    /// type supports track metadata (RAOP only today). Cloning it is
    /// cheap and lets the caller send without holding the session lock.
    pub fn metadata_handle(&self) -> Option<crate::airplay::session::MetadataHandle> {
        match self {
            ActiveSession::AirPlay(s) => s.metadata_handle(),
            ActiveSession::AirPlay2(_) | ActiveSession::Upnp(_) | ActiveSession::Sendspin(_) | ActiveSession::MusicAssistant(_) => None,
        }
    }

    /// The speaker ended the session itself in a way that rules out an
    /// automatic reconnect (Sendspin goodbye such as "another_server").
    pub fn ended_by_speaker(&self) -> Option<String> {
        match self {
            ActiveSession::Sendspin(s) => s.ended_by_speaker(),
            ActiveSession::AirPlay(s) if s.ended_while_reachable() => Some(ENDED_WHILE_REACHABLE.into()),
            ActiveSession::AirPlay2(s)
                if s.fault().is_some_and(|f| f.code == crate::airplay::ap2_health::ENDED_BY_RECEIVER) =>
            {
                Some(ENDED_WHILE_REACHABLE.into())
            }
            _ => None,
        }
    }

    /// The speaker says it plays another source now (Sendspin reports it).
    pub fn reports_busy(&self) -> bool {
        match self {
            ActiveSession::Sendspin(s) => s.reports_busy(),
            _ => false,
        }
    }

    /// Short protocol label for diagnostics.
    pub fn transport_label(&self) -> &'static str {
        match self {
            ActiveSession::Upnp(_) => "UPnP",
            ActiveSession::AirPlay(_) => "AirPlay (RAOP)",
            ActiveSession::AirPlay2(_) => "AirPlay 2",
            ActiveSession::Sendspin(_) => "Sendspin",
            ActiveSession::MusicAssistant(_) => "Music Assistant (Sendspin)",
        }
    }

    /// True if the session has died mid-stream (dropped receiver). Only
    /// the push paths can tell: nothing watches whether a UPnP speaker
    /// still fetches the stream, so a UPnP session is never reported dead
    /// here, even when its speaker has stopped.
    pub fn is_dead(&self) -> bool {
        match self {
            ActiveSession::Upnp(_) => false,
            ActiveSession::AirPlay(s) => s.is_dead(),
            ActiveSession::AirPlay2(s) => s.is_dead(),
            ActiveSession::Sendspin(s) => s.is_dead(),
            ActiveSession::MusicAssistant(_) => false,
        }
    }

    /// The final fault reported by AirPlay 2. If authentication failed,
    /// the engine treats the receiver as asking for its code. For other
    /// faults it retries while sound is playing.
    pub fn final_fault(&self) -> Option<crate::airplay::ap2_health::Ap2Fault> {
        match self {
            ActiveSession::AirPlay2(s) => s
                .fault()
                .filter(|f| !f.retryable && f.code != crate::airplay::ap2_health::ENDED_BY_RECEIVER),
            ActiveSession::Upnp(_)
            | ActiveSession::AirPlay(_)
            | ActiveSession::Sendspin(_)
            | ActiveSession::MusicAssistant(_) => None,
        }
    }

    /// `(resend requests, packets re-sent)` for the AirPlay push paths;
    /// `None` for UPnP, where the speaker pulls over TCP and nothing is
    /// retransmitted at our level.
    pub fn resend_stats(&self) -> Option<(u64, u64)> {
        match self {
            ActiveSession::Upnp(_) => None,
            ActiveSession::AirPlay(s) => Some(s.resend_stats()),
            ActiveSession::AirPlay2(s) => Some(s.resend_stats()),
            ActiveSession::Sendspin(_) | ActiveSession::MusicAssistant(_) => None,
        }
    }
}

pub type SharedSession = Arc<Mutex<Option<ActiveSession>>>;

/// Sets level (millibels) and mute on our endpoint, moving the Windows
/// volume slider.
pub type WindowsVolumePush = Box<dyn Fn(i32, bool) -> Result<()> + Send + Sync>;

/// How the app moves Windows' slider for its own output: through Windows'
/// endpoint volume when the engine runs on the real PC, else the driver's
/// push.
#[derive(Default)]
pub struct WindowsSlider {
    driver: OnceLock<WindowsVolumePush>,
    endpoint: OnceLock<Box<dyn Fn(u32, bool) -> bool + Send + Sync>>,
}

/// What a volume or mute change needs to reach the bound speaker, cloned
/// out of the session so the network round trip runs without the session
/// lock — the window takes that lock several times on every repaint.
#[derive(Clone)]
pub enum VolumeControl {
    /// RenderingControl on the speaker itself.
    Upnp(String),
    /// GroupRenderingControl on a Sonos group's coordinator.
    UpnpGroup(String),
    AirPlay(crate::airplay::session::VolumeHandle),
    AirPlay2(crate::airplay::ap2_session::VolumeHandle),
    Sendspin(crate::sendspin::server::VolumeHandle),
}

impl VolumeControl {
    fn upnp(r: &Renderer) -> Self {
        match r.group_volume_control_url() {
            Some(url) => Self::UpnpGroup(url.to_owned()),
            None => Self::Upnp(r.rendering_control_control_url.clone()),
        }
    }

    pub fn set_volume_pct(&self, pct: u32) -> Result<()> {
        match self {
            VolumeControl::Upnp(url) => upnp::set_volume(url, pct),
            VolumeControl::UpnpGroup(url) => upnp::set_group_volume(url, pct),
            VolumeControl::AirPlay(h) => h.set_volume_pct(pct),
            VolumeControl::AirPlay2(h) => h.set_volume_pct(pct),
            VolumeControl::Sendspin(h) => h.set_volume_pct(pct),
        }
    }

    pub fn set_mute(&self, muted: bool) -> Result<()> {
        match self {
            VolumeControl::Upnp(url) => upnp::set_mute(url, muted),
            VolumeControl::UpnpGroup(url) => upnp::set_group_mute(url, muted),
            VolumeControl::AirPlay(h) => h.set_mute(muted),
            VolumeControl::AirPlay2(h) => h.set_mute(muted),
            VolumeControl::Sendspin(h) => h.set_mute(muted),
        }
    }
}

/// Sends volume and mute changes to the bound speaker from a thread of its
/// own. Each send runs on a [`VolumeControl`] cloned under a brief session
/// lock, so a slow or unreachable speaker (a SOAP call can wait seconds)
/// holds up neither the window, which takes the session lock on every
/// repaint, nor the driver events queued behind a volume change. Changes
/// that arrive during a send wait in [`PendingCommands`], where a slider
/// drag collapses into its newest level. Each change is for the session
/// bound when it was queued; one left over when another speaker has been
/// bound since is dropped, never sent to the new speaker.
///
/// When a run of level changes is over, the thread also settles where the
/// speaker ended up (see [`VolumeSync::speaker_settle`]) and moves the
/// Windows slider there if that is not where we left it. Apart from the
/// level read as a session starts, only UPnP speakers' level reports
/// (GENA events) reach [`VolumeSync`]; for AirPlay and Sendspin the settle
/// finds nothing to do.
pub struct SpeakerControl {
    shared: Arc<SpeakerControlShared>,
}

struct SpeakerControlShared {
    pending: Mutex<PendingCommands>,
    wake: Condvar,
    closed: AtomicBool,
    /// Number of the bound session; changed whenever one is bound, under
    /// the session lock.
    session: AtomicU64,
}

impl SpeakerControl {
    fn new(session: SharedSession, vsync: Arc<VolumeSync>, windows: Arc<WindowsSlider>) -> Self {
        let shared = Arc::new(SpeakerControlShared {
            pending: Mutex::new(PendingCommands::default()),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
            session: AtomicU64::new(0),
        });
        let worker = shared.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("stream-to-speaker-volume".to_string())
            .spawn(move || run_speaker_control(&worker, &session, &vsync, &windows))
        {
            warn!("speaker volume thread failed to start: {}", e);
        }
        Self { shared }
    }

    /// Queue a change for the bound speaker; returns at once.
    pub fn send(&self, cmd: SpeakerCommand) {
        let session = self.shared.session.load(Ordering::Acquire);
        self.shared.pending.lock().unwrap().push(session, cmd);
        self.shared.wake.notify_one();
    }

    /// A new session is being bound (call under the session lock). Returns
    /// its number.
    fn session_bound(&self) -> u64 {
        self.shared.session.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Number of the session bound last.
    fn session_number(&self) -> u64 {
        self.shared.session.load(Ordering::Acquire)
    }
}

impl Drop for SpeakerControl {
    fn drop(&mut self) {
        let _pending = self.shared.pending.lock().unwrap();
        self.shared.closed.store(true, Ordering::Release);
        self.shared.wake.notify_all();
    }
}

/// Set the Windows slider to `level`: through Windows' endpoint volume when
/// the engine set that up, else through the driver if it is there.
fn push_windows_level(windows: &WindowsSlider, level: u32, muted: bool) {
    if windows.endpoint.get().is_some_and(|set| set(level.min(100), muted)) {
        return;
    }
    if let Some(push) = windows.driver.get() {
        if let Err(e) = push(sonos_to_millibels(level), muted) {
            warn!("failed to push volume to Windows: {:#}", e);
        }
    }
}

fn run_speaker_control(
    shared: &SpeakerControlShared,
    session: &SharedSession,
    vsync: &VolumeSync,
    windows: &WindowsSlider,
) {
    // A level went to the speaker since the last settle.
    let mut settle_due = false;
    loop {
        let next = {
            let mut pending = shared.pending.lock().unwrap();
            loop {
                if shared.closed.load(Ordering::Acquire) {
                    return;
                }
                if let Some(next) = pending.pop() {
                    break Some(next);
                }
                if !settle_due {
                    pending = shared.wake.wait(pending).unwrap();
                    continue;
                }
                match vsync.speaker_run_ends_in() {
                    Some(left) => pending = shared.wake.wait_timeout(pending, left).unwrap().0,
                    None => break None,
                }
            }
        };
        let Some((number, cmd)) = next else {
            settle_due = false;
            if let Some(level) = vsync.speaker_settle() {
                info!("speaker -> windows: volume {} (where the speaker settled)", level);
                push_windows_level(windows, level, vsync.is_muted());
            }
            continue;
        };
        let control = {
            let guard = session.lock().unwrap();
            if shared.session.load(Ordering::Acquire) != number {
                debug!("dropping {:?} queued for a speaker no longer bound", cmd);
                None
            } else {
                let control = guard.as_ref().and_then(|s| s.volume_control());
                // Under the session lock, so a speaker bound meanwhile
                // starts from a clean run.
                if let (Some(_), SpeakerCommand::Volume(level)) = (&control, cmd) {
                    vsync.speaker_send_start(level);
                }
                control
            }
        };
        let Some(control) = control else { continue };
        let result = match cmd {
            SpeakerCommand::Volume(level) => {
                // AirPlay uses its volume for mute. Keep the level for the next unmute.
                let result = match &control {
                    VolumeControl::AirPlay(h) if vsync.is_muted() => Ok(h.remember_volume_pct(level)),
                    VolumeControl::AirPlay2(h) if vsync.is_muted() => Ok(h.remember_volume_pct(level)),
                    _ => control.set_volume_pct(level),
                };
                vsync.speaker_send_done();
                settle_due = true;
                result
            }
            SpeakerCommand::Mute(muted) => control.set_mute(muted),
        };
        if let Err(e) = result {
            match cmd {
                SpeakerCommand::Volume(level) => warn!("set speaker volume to {} failed: {:#}", level, e),
                SpeakerCommand::Mute(muted) => warn!("set speaker mute to {} failed: {:#}", muted, e),
            }
        }
    }
}

/// How long quitting waits for a connect still in flight to finish (it then
/// tears down what it set up).
const QUIT_CONNECT_WAIT: Duration = Duration::from_secs(5);
/// How long quitting waits for the speaker to answer its Stop or TEARDOWN.
const QUIT_TEARDOWN_WAIT: Duration = Duration::from_secs(2);

/// What [`App::take_stream_off_number`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TakenOff {
    /// The UPnP speaker was told Stop; its session is kept.
    Stopped,
    /// The receiver (this id) was let go.
    LetGo(String),
}

/// What [`ActiveSession::ended_by_speaker`] says of an AirPlay receiver
/// that ended this PC's session while it still answers.
pub(crate) const ENDED_WHILE_REACHABLE: &str = "ended_while_reachable";

pub(crate) const CODE_WINDOW_CLOSED: &str = "The code window closed before the speaker was paired.";

/// The apps whose song titles go to a speaker, by the start of their
/// program or app name, lower case: music players. Browsers, video
/// players and every other app are left out, whatever they play.
pub const MUSIC_APPS: &[&str] = &[
    "spotify",
    "applemusic",
    "appleinc.applemusic",
    "itunes",
    "appleinc.itunes",
    "tidal",
    "deezer",
    "amazon music",
    "amazonmusic",
    "qobuz",
    "musicbee",
    "foobar2000",
    "aimp",
    "winamp",
    "plexamp",
    "microsoft.zunemusic",
];

/// Whether `app` (a program or app name, as Windows gives it) is one of
/// [`MUSIC_APPS`].
pub fn is_music_app(app: &str) -> bool {
    music_app(app).is_some()
}

/// The music app `app` belongs to. A Store app's ID (`AppleInc.AppleMusicWin_…`)
/// and its program's name (`AppleMusic.exe`) give the same answer, so that
/// a media session and the sound of its own program are one app.
fn music_app(app: &str) -> Option<&'static str> {
    let app = app.trim().to_ascii_lowercase();
    let app = app.rsplit(['\\', '/']).next().unwrap_or_default();
    let entry = MUSIC_APPS.iter().copied().find(|m| app.starts_with(m))?;
    Some(match entry {
        "appleinc.applemusic" => "applemusic",
        "appleinc.itunes" => "itunes",
        "amazon music" => "amazonmusic",
        other => other,
    })
}

/// One player, as song titles see it.
pub use crate::now_playing::Session as TitleSource;

/// The song title to show on the speaker: the title of a music app that
/// plays, while no other app's real sound plays on the app's output.
/// None otherwise: a paused or stopped player, a browser, a video player,
/// an app Windows doesn't name. Whether titles go at all (the switch, a
/// speaker that plays this PC's sound) is the caller's.
pub fn title_to_send(sources: &[TitleSource], sounding_apps: &[String]) -> Option<crate::now_playing::NowPlaying> {
    sources.iter().filter(|s| s.playing).find_map(|s| {
        let source = music_app(s.app.as_deref()?)?;
        if sounding_apps.iter().any(|app| music_app(app) != Some(source)) {
            return None;
        }
        s.title.clone().filter(|t| !t.is_empty())
    })
}

/// Windows' current media session as a source of song titles.
pub fn title_sources() -> Vec<TitleSource> {
    crate::now_playing::current_session()
        .into_iter()
        .collect()
}

/// An AirPlay speaker's level from this PC is kept this long: within it,
/// it starts at that level again.
pub const LEVEL_KEPT_FOR: Duration = Duration::from_secs(3 * 60 * 60);
/// After [`LEVEL_KEPT_FOR`], a speaker starts at no more than this.
pub const LEVEL_LATER_AT_MOST: u8 = 30;

/// The level a speaker whose level can't be read (AirPlay) starts at:
/// the level last set from this PC (`level`, at `level_at`) if that was
/// within [`LEVEL_KEPT_FOR`] of `now`, later the lower of it and
/// [`LEVEL_LATER_AT_MOST`]; the first time
/// [`crate::engine::place::numbers::LEVEL_FIRST`]. Never above `limit`, the
/// speaker's *Maximum volume from this PC*. All times in seconds since
/// 1970.
pub fn start_level(level: Option<u8>, level_at: Option<u64>, now: u64, limit: Option<u8>) -> u8 {
    let lately = level_at.is_some_and(|at| now.saturating_sub(at) <= LEVEL_KEPT_FOR.as_secs());
    let level = match level {
        Some(l) if lately => l,
        Some(l) => l.min(LEVEL_LATER_AT_MOST),
        None => crate::engine::place::numbers::LEVEL_FIRST,
    };
    level.min(100).min(limit.unwrap_or(100))
}

/// Seconds since 1970.
fn unix_seconds() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

/// What a connect a newer choice superseded ends with (never shown).
pub(crate) const SUPERSEDED: &str = "superseded by a newer choice";

/// How long an error stays in the GUI's banner (the usual toast time).
const ERROR_BANNER_TTL: Duration = Duration::from_secs(8);
/// Start of the notice shown when a speaker is picked mid-connect.
const STILL_CONNECTING_PREFIX: &str = "Still connecting to ";

/// Transport-agnostic display info for the bound speaker (UPnP, AirPlay 1
/// or AirPlay 2), used by the GUI status banner and the tray label.
pub struct SelectedSpeaker {
    pub friendly_name: String,
    pub ip: IpAddr,
}

/// Stable summary of the runtime config that the GUI / tray can render.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// `--advertise-ip`: given to every speaker and named in the web UI's
    /// link. Without it each speaker gets the address this PC uses to reach
    /// it (see [`App::local_ip_toward`]), and the link the address worked
    /// out when it is shown (see [`App::this_pc_address`]).
    pub advertise_ip: Option<IpAddr>,
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

/// Result of one update check, for the Help menu's status line.
#[derive(Clone, Debug)]
pub enum UpdateOutcome {
    UpToDate,
    Available(ReleaseInfo),
    /// No release tag baked into this exe → nothing to compare against.
    DevBuild,
    Failed(String),
}

/// The window's AirPlay password dialog, as it opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordDialog {
    /// The entry it was opened for.
    pub id: String,
    /// Its name, for the title.
    pub name: String,
    /// What is typed is used for one connection and never stored (the
    /// entry announced another speaker's identity when the dialog opened).
    pub one_session: bool,
}

enum TypedPassword {
    Once(String),
    Saved,
}

/// In-memory update-check state (the persistent part lives in `UserConfig`).
#[derive(Clone, Debug, Default)]
pub struct UpdateState {
    /// A check is in flight (Help menu shows "Checking…").
    pub checking: bool,
    /// Outcome + unix time of the most recent check this run.
    pub last: Option<(UpdateOutcome, u64)>,
}

pub struct App {
    /// Resolved-at-startup configuration. Immutable after `new`.
    pub config: AppConfig,

    // ---- Discovery + active session ----
    pub discovery: Option<Arc<DiscoveryState>>,
    pub airplay_discovery: Option<Arc<AirPlayDiscoveryState>>,
    /// What the password dialog supplied for the next connect to each receiver.
    typed_airplay_passwords: Mutex<std::collections::HashMap<String, TypedPassword>>,
    /// AirPlay receivers (by id) that asked for a password in this run,
    /// whether or not they announce one: picking them opens the password
    /// dialog, as for one that does.
    airplay_asked_password: Mutex<std::collections::HashSet<String>>,
    pub session: SharedSession,
    /// Remembered speaker id so Disable→Enable can rebind to the same one.
    pub last_speaker_id: Mutex<Option<String>>,

    // ---- Audio data plumbing ----
    pub hub: Arc<StreamHub>,
    pub vsync: Arc<VolumeSync>,
    speaker_control: SpeakerControl,
    /// Sets the Windows slider: (millibels, muted). Registered at startup
    /// when the driver is there.
    windows_volume: Arc<WindowsSlider>,

    // ---- Runtime atomics (audio hot path reads these every packet) ----
    /// What the driver last said about Windows' stream on our device:
    /// running, or stopped. Set by the driver-event consumer only; true at
    /// launch and while its events can't be read (unknown). For the status
    /// shown: the audio loop doesn't depend on it.
    pub stream_active: Arc<AtomicBool>,
    /// Why the audio thread can't read this PC's sound (the sound device
    /// was disabled, removed or updated), while it tries to open it again.
    sound_input_lost: Mutex<Option<String>>,
    /// User toggle. False ⇒ no session, audio goes into the void, and no
    /// device but this PC may pull `/stream.raw` (the stream gate holds
    /// this same flag).
    pub streaming_enabled: Arc<AtomicBool>,
    /// The web UI / JSON API: whether it is on, and the secret its link
    /// and API key carry (see `web_access`). Starts on with `--web`; the
    /// user can flip it at runtime from the GUI. The `/stream.raw` route
    /// is always served (the speaker needs it); only the user-facing
    /// routes (the page under `/r/<key>/`, `/api/*`) honour this.
    pub web_access: Arc<WebAccess>,
    /// Each speaker's delay: what was removed or added, how it applies
    /// (see `delay`).
    pub delay: crate::delay::DelayEngine,
    pub rate_fudge_ppm: Arc<AtomicI32>,
    pub silence_pace_ms: Arc<AtomicU64>,
    pub latency_adjust_step_frames: Arc<AtomicU32>,
    /// Equalizer hand-off to the audio loop (designs built off-thread).
    pub eq: Arc<EqControl>,

    // ---- Stats (best-effort, advisory) ----
    pub packets_published_total: Arc<AtomicU64>,
    /// Of `packets_published_total`, the silence generated while Windows
    /// had no stream running.
    pub idle_silence_packets: Arc<AtomicU64>,
    /// When the audio loop last got a packet from Windows, and the last
    /// one that carried sound, in ms since `started_at` plus one
    /// (0 = never).
    source_packet_at_ms: AtomicU64,
    source_sound_at_ms: AtomicU64,
    pub started_at: Instant,
    /// Id of the bound session and when it came up (session uptime).
    session_started: Mutex<Option<(String, Instant)>>,
    /// Automatic reconnects since the user last picked a speaker.
    reconnects: AtomicU32,
    /// The last dropped / failed connection, with the counters as they
    /// were just before it. Kept until the next fault, across reconnects.
    last_fault: Mutex<Option<Fault>>,

    // ---- Lifecycle ----
    pub shutdown: Arc<AtomicBool>,
    /// Quit without waiting for the speaker (a second Ctrl+C).
    quit_now: AtomicBool,
    quit_hook_finished: AtomicBool,
    /// Run once when quitting begins (see [`App::on_quit`]).
    on_quit: OnceLock<Box<dyn Fn(crate::model::Surface) + Send + Sync>>,
    /// Update-check state; see `update_check` and [`App::spawn_update_checker`].
    pub update: Mutex<UpdateState>,

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
    pub user_config: crate::user_config::SettingsCell,
    pub(crate) delay_shortcuts: Mutex<std::sync::Weak<Mutex<crate::engine::place_driver::DelayShortcuts>>>,
    /// Whether settings saves reach `config.json` (shared with the settings).
    settings_health: crate::user_config::SaveHealth,
    /// What loading the settings found that the window says once, until
    /// dismissed.
    settings_notice: Mutex<Option<crate::user_config::LoadNotice>>,
    /// When a failed save was last tried again (see
    /// [`App::retry_settings_save_if_due`]).
    settings_retry_at: Mutex<Option<Instant>>,
    /// A retry's thread is running: no second one meanwhile.
    settings_retrying: Arc<AtomicBool>,

    /// Most recent user-facing error message and when it was recorded.
    /// Surfaced by the GUI as a transient inline banner for ~8 s, so a
    /// failed action is seen, not only written to the log file.
    pub last_error: Mutex<Option<(String, Instant)>>,

    /// Friendly name of the speaker a background connect is currently
    /// running for (None when idle). Session bring-up does seconds of
    /// blocking network I/O — pairing, SETUPs, fallbacks — so the GUI
    /// runs it on a worker thread and shows "Connecting…" off this.
    pub connecting: Mutex<Option<String>>,
    /// What an in-flight UPnP connect is doing, for the banner's second
    /// line (see [`ConnectStep::text`]); None for the usual line.
    connect_detail: Mutex<Option<String>>,

    /// Counts people's picks, including ones that fail. The engine checks
    /// this count as it handles a pending `--player` request, before
    /// starting its switch. That request does not itself advance the count.
    user_picks: AtomicU64,

    /// Where a `--player` wait stands (see [`App::connect_when_listed`]).
    player_wish: Mutex<Option<PlayerWish>>,

    /// The stream gate for `/stream.raw` (see `stream_gate.rs`): no device
    /// but this PC while streaming is disabled, and with privacy mode on
    /// only the speaker being played on. Sessions hold grants in it; the
    /// HTTP threads consult it per request.
    pub stream_gate: Arc<StreamGate>,

    /// The one in-flight HomeKit PIN pairing ceremony, if any. Single
    /// source of truth for the whole ceremony lifecycle: reserving the
    /// slot IS the "only one ceremony at a time" lock, the `AwaitingPin`
    /// stage is what the GUI's modal mirrors, and any stage blocks
    /// re-selecting the same device (a second transient pair-setup would
    /// reset the accessory's single HAP pairing session mid-ceremony).
    /// Cleared by the ceremony's RAII guard on every exit path.
    pin_ceremony: Mutex<Option<PinCeremonyState>>,

    /// Sendspin players + the Music Assistant source (`sendspin_app`).
    pub(crate) sendspin: crate::sendspin_app::SendspinAppState,
    /// Receiver records built for manually added AirPlay speakers at their
    /// last connect (the probe result), keyed by id — so the PIN ceremony
    /// and other lookups by id find them without re-probing.
    manual_resolved: Mutex<std::collections::HashMap<String, AirPlayRenderer>>,

    /// Discovery's adapter state as last applied (see `discovery_net`):
    /// every adapter, one adapter, or paused because it is down.
    discovery_scope: Mutex<Resolved>,
    /// The machine's adapters as of the last network check, for the
    /// adapter selector (refreshed every couple of seconds off-thread).
    adapters_cache: Mutex<Vec<NetAdapter>>,
    /// The AirPlay mDNS browser, rebuilt when the chosen adapter changes
    /// (Sendspin's is in `sendspin`).
    mdns: Mutex<Option<MdnsController>>,
    /// Serialises scope changes (the monitor thread vs. a settings edit).
    scope_apply: Mutex<()>,
    /// Why AirPlay discovery couldn't start, if it couldn't.
    discovery_error: Mutex<Option<String>>,
    /// Where today's entry points (the window, the tray, the web API, the
    /// terminal prompt) send their commands once the engine runs.
    pub(crate) command_sink: OnceLock<CommandSink>,
    pub(crate) engine_port: OnceLock<std::sync::Weak<crate::model::PortCore>>,
    engine_connects: Mutex<EngineConnects>,
    engine_connect_ended: Condvar,
    /// The session the chime plays on and its next frame.
    test_sound: Mutex<Option<(u64, usize)>>,
    pub(crate) capture_tap: std::sync::OnceLock<crate::engine::place_driver::tap::Tap>,
    /// Devices refused the stream lately, newest first.
    refused: Mutex<Vec<RefusedDevice>>,
    /// Devices let in by the person for this run.
    allowed: Mutex<Vec<(IpAddr, crate::stream_gate::Grant)>>,
    /// The bound UPnP speaker's latest transport event, until the engine
    /// reads it.
    transport_change: Mutex<Option<crate::gena::TransportChange>>,
    /// The bound Sonos room's latest topology event, until the engine
    /// reads it.
    topology: Mutex<Option<Vec<crate::sonos::ZoneGroup>>>,
    /// What speakers said of themselves, until the engine reads it.
    pub signals: crate::engine::signals::SignalLog,
    /// The programs with real sound on the app's output at the latest sample.
    pub(crate) sounding_apps: Mutex<Vec<String>>,
    /// The speaker this PC played on last and when that ended: its
    /// announcement may still say it receives for a moment.
    last_ended: Mutex<Option<(String, Instant)>>,
    device_uses: Arc<DeviceUses>,
}

/// Takes a command for the engine (see [`App::set_command_sink`]); the
/// ticket brings its outcome.
pub type CommandSink = Box<dyn Fn(crate::model::Surface, crate::model::Command) -> crate::model::Ticket + Send + Sync>;

/// A device that asked for the stream and was refused (privacy mode).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedDevice {
    /// The speaker the app was connected to when the device asked.
    pub speaker: Option<String>,
    /// The number of that connection's session.
    pub session: u64,
    pub address: IpAddr,
    /// Its name when discovery knows it, else its address.
    pub name: String,
    pub at: std::time::SystemTime,
}

/// The most refused devices remembered.
const REFUSALS_KEPT: usize = 20;

/// The test sound: two notes, 0.3 s each, at -12 dBFS, faded in and out.
const TEST_SOUND_NOTES: [f32; 2] = [880.0, 1318.5];
const TEST_SOUND_NOTE_FRAMES: usize = WIRE_SAMPLE_RATE as usize * 3 / 10;
/// The chime's length in frames.
const CHIME_FRAMES: usize = TEST_SOUND_NOTE_FRAMES * TEST_SOUND_NOTES.len();

/// Where the stream server serves the test chime as a file of its own.
pub const CHIME_PATH: &str = "/chime.wav";

/// The chime's sample at frame `pos` (of [`CHIME_FRAMES`]).
fn chime_at(pos: usize) -> i16 {
    let note = pos / TEST_SOUND_NOTE_FRAMES;
    let at = pos % TEST_SOUND_NOTE_FRAMES;
    let fade = (TEST_SOUND_NOTE_FRAMES / 20) as f32;
    let envelope = (at as f32 / fade).min((TEST_SOUND_NOTE_FRAMES - at) as f32 / fade).min(1.0);
    let t = at as f32 / WIRE_SAMPLE_RATE as f32;
    ((std::f32::consts::TAU * TEST_SOUND_NOTES[note] * t).sin() * envelope * 0.25 * i16::MAX as f32) as i16
}

/// The chime, `channels` interleaved, with a fifth of a second of silence
/// before it (some speakers clip the very start of what they are given).
pub fn chime_pcm(channels: u16) -> Vec<i16> {
    let lead = WIRE_SAMPLE_RATE as usize / 5;
    let ch = channels.max(1) as usize;
    let mut out = vec![0i16; lead * ch];
    for pos in 0..CHIME_FRAMES {
        out.extend(std::iter::repeat(chime_at(pos)).take(ch));
    }
    out
}

/// DIDL-Lite for the chime's file.
fn chime_didl(uri: &str) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"chime\" parentID=\"0\" restricted=\"1\"><dc:title>{PRODUCT_NAME}</dc:title>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"http-get:*:audio/wav:*\">{}</res></item></DIDL-Lite>",
        upnp_xml_escape(uri)
    )
}

/// `s` with XML's special characters escaped.
fn upnp_xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The engine's connects by speaker id: the number of the latest, and how
/// it ended once it has (`Err` with the words to show).
#[derive(Default)]
pub(crate) struct EngineConnects {
    by_id: std::collections::HashMap<String, (u64, Option<Result<(), String>>)>,
}

/// The place a speaker id names.
pub(crate) fn place_of(id: &str) -> crate::model::Place {
    if id == crate::sendspin_app::MA_OUTPUT_ID {
        crate::model::Place::MusicAssistant
    } else {
        crate::model::Place::Speaker(crate::model::SpeakerId::new(id))
    }
}

/// Discovery-network state for the settings UI.
#[derive(Clone, Debug)]
pub struct DiscoveryNetStatus {
    pub scope: Resolved,
    /// The saved choice (`None` = every adapter).
    pub selected: Option<SavedAdapter>,
    /// Set when `--advertise-ip` / `--bind` pins SSDP to one address; the
    /// adapter choice is then ignored.
    pub pinned: Option<Ipv4Addr>,
    /// AirPlay or Sendspin discovery failed to start (one line each).
    pub error: Option<String>,
}

/// Take a session under the session lock and remember which speaker the connect let go.
fn take_for_connect(slot: &mut Option<ActiveSession>, number: u64, job: Option<&crate::engine::cancel::Binding>) -> Option<ActiveSession> {
    let session = slot.take()?;
    if let Some(job) = job {
        let taken = (session.stable_id(), number);
        let mut released = job.released.lock().unwrap();
        if !released.contains(&taken) { released.push(taken); }
    }
    Some(session)
}

/// A PIN pairing ceremony in flight (see [`App::begin_pin_pairing`]).
struct PinCeremonyState {
    /// Stable id of the receiver being paired.
    id: String,
    /// Friendly name, for the modal + toasts.
    name: String,
    stage: PinStage,
    /// The person cancelled the dialog.
    cancelled: Arc<AtomicBool>,
    /// What the last code, or the last ask for a new one, came to.
    code_try: Option<CodeTry>,
}

/// What became of the last code typed for a pairing, or of the last ask
/// for a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeTry {
    /// The code didn't work; the app asked the device to show a new one,
    /// and `shown`: it accepted.
    Wrong { shown: bool },
    /// The person asked for a new code; `shown`: the device accepted.
    NewCode { shown: bool },
}

/// What the person gave a pairing waiting for its code.
enum PinAnswer {
    Code(String),
    /// Ask the device for a new code.
    NewCode,
    Cancel,
}

enum PinStage {
    /// Connecting + /pair-pin-start — the receiver isn't showing a PIN yet.
    Starting,
    /// The ceremony waits here even if the code expired or the TV failed to show it.
    AwaitingPin { answers: crossbeam_channel::Sender<PinAnswer>, expires: Instant },
    /// PIN received — pair-setup M1–M6 is running.
    Pairing,
}

/// Result of one AirPlay bring-up attempt (one entry from
/// `airplay_attempts`). `NeedsPin` is distinguished from a generic failure
/// so `start_airplay` can surface the PIN requirement only after every
/// path — including the fallback legacy RAOP that most Apple TVs also
/// expose — has failed.
#[derive(Debug)]
enum AttemptError {
    /// Only the one-time PIN ceremony can get this receiver going.
    NeedsPin(PinReason),
    /// Any other failure — the message is already user-facing.
    Other(String),
}

/// Why a receiver needs the PIN ceremony — what the user is told. In none
/// of these cases is the stored pairing deleted: a successful ceremony
/// replaces it, and the user can forget it.
#[derive(Debug, Clone, PartialEq)]
enum PinReason {
    /// It asks for PIN pairing (an AP2 470) and has no stored pairing.
    Required,
    /// Its stored pairing in the config can't be parsed; nothing was sent.
    Damaged(String),
    /// Something answered at `addr` but didn't prove it is the paired
    /// device: another device there, an impostor, or the device reset.
    NotProven { addr: IpAddr, detail: String },
    /// It proved it is the paired device, then refused this PC's pairing
    /// (e.g. removed on the device or in the Home app, or a reset).
    Refused,
}

/// How much of a receiver's answer a message in the window shows. The
/// answer can run to a MiB (an error body is dumped whole); `pair_member`
/// logs it in full.
const RECEIVER_DETAIL_CHARS: usize = 200;

/// `s` cut to at most `max` characters, with "…" when cut.
fn clip_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

/// Map a failed AirPlay 2 bring-up to the attempt's outcome. Kept free of
/// side effects: whatever the receiver answered, the stored pairing stays.
fn ap2_attempt_error(e: Ap2StartError, addr: IpAddr) -> AttemptError {
    match e {
        // 470 with no stored keys: typed, so start_airplay surfaces the
        // PIN requirement only after the RAOP fallback has also failed.
        Ap2StartError::NeedsPin => AttemptError::NeedsPin(PinReason::Required),
        Ap2StartError::PairingNotProven(e) => AttemptError::NeedsPin(PinReason::NotProven {
            addr,
            detail: clip_chars(&format!("{:#}", e), RECEIVER_DETAIL_CHARS),
        }),
        Ap2StartError::PairingRefused(_) => AttemptError::NeedsPin(PinReason::Refused),
        Ap2StartError::Other(e) => AttemptError::Other(format!("{:#}", e)),
    }
}

/// What to tell the user when `name` needs the PIN ceremony, by `reason`.
/// `interactive`: the ceremony is starting now (a click in the window);
/// otherwise the message says where to start it.
fn pin_pairing_message(name: &str, reason: &PinReason, interactive: bool) -> String {
    let why = match reason {
        PinReason::Required => {
            return if interactive {
                format!("{} needs one-time pairing — enter the PIN shown on its screen.", name)
            } else {
                format!(
                    "{} needs one-time PIN pairing — open the {} window and click it in the speaker \
                     list to pair.",
                    name, PRODUCT_NAME
                )
            };
        }
        PinReason::Damaged(detail) => {
            format!("The stored pairing with {} is damaged and can't be used ({}).", name, detail)
        }
        PinReason::NotProven { addr, detail } => format!(
            "Couldn't verify {name}: the device at {addr} answered but didn't prove it is the {name} \
             paired with this PC ({detail}). Another device may have its address, or {name} was \
             reset. The stored pairing is kept."
        ),
        PinReason::Refused => format!(
            "{name} no longer accepts this PC's pairing — it may have been removed on {name} or in \
             the Home app, or {name} was reset."
        ),
    };
    let next = match (reason, interactive) {
        (PinReason::NotProven { .. }, true) => {
            format!("If {} shows a PIN, enter it to pair again; if not, cancel.", name)
        }
        (PinReason::NotProven { .. }, false) => format!(
            "If {} was reset, open the {} window and click it in the speaker list to pair again.",
            name, PRODUCT_NAME
        ),
        (_, true) => "To pair again, enter the PIN shown on its screen.".to_string(),
        (_, false) => format!(
            "To pair again, open the {} window and click it in the speaker list.",
            PRODUCT_NAME
        ),
    };
    format!("{} {}", why, next)
}

/// The sentence to add where PIN pairing with `name` couldn't start, failed
/// or may show no PIN while this PC has a stored pairing with it
/// (`has_pairing`); empty without one. A device that stopped asking for
/// verification may refuse PIN pairing too, and a device with a stored
/// pairing is never tried without it, so forgetting the pairing is then
/// the only way to connect.
fn forget_pairing_hint(name: &str, has_pairing: bool) -> String {
    if !has_pairing {
        return String::new();
    }
    format!(
        " If {name} no longer asks for device verification, forget its pairing to connect without a \
         PIN: ⚙ next to it in the speaker list → Forget pairing."
    )
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
        reason: PinReason,
        /// The other attempted paths' failures, pre-joined (may be empty).
        details: String,
    },
    Msg(String),
}

fn check_job() -> Result<(), String> {
    if crate::engine::cancel::cancelled() { Err(SUPERSEDED.into()) } else { Ok(()) }
}

fn chime_airplay<T>(
    in_window: bool,
    mut connect: impl FnMut() -> Result<T, SelectFailure>,
    pair: impl FnOnce(String, String) -> Result<(), String>,
) -> Result<T, String> {
    check_job()?;
    let mut result = connect();
    if in_window {
        if let Err(SelectFailure::NeedsPin { id, name, .. }) = result {
            check_job()?;
            pair(id, name)?;
            check_job()?;
            result = connect();
        }
    }
    result.map_err(|e| match e {
        SelectFailure::Msg(m) => m,
        SelectFailure::NeedsPin { name, .. } => format!("{name} needs its code"),
    })
}

#[derive(Default)]
struct DeviceUses {
    devices: Mutex<Vec<Arc<SessionDevice>>>,
    ended: Condvar,
}

impl DeviceUses {
    fn register(self: &Arc<Self>, devices: &mut Vec<Arc<SessionDevice>>, device: SessionDevice) -> DeviceUse {
        let device = Arc::new(device);
        devices.push(device.clone());
        DeviceUse { uses: self.clone(), device }
    }
}

struct SessionDevice {
    id: String,
    speaker: Option<crate::model::SpeakerId>,
    renderer: Option<Renderer>,
}

/// Held from before a connect starts playback until it binds or finishes stopping,
/// and from taking a bound session until its teardown ends.
struct DeviceUse {
    uses: Arc<DeviceUses>,
    device: Arc<SessionDevice>,
}

impl Drop for DeviceUse {
    fn drop(&mut self) {
        self.uses.devices.lock().unwrap().retain(|d| !Arc::ptr_eq(d, &self.device));
        self.uses.ended.notify_all();
    }
}

pub(crate) struct ReleasingSession {
    session: ActiveSession,
    device: DeviceUse,
}

impl ReleasingSession {
    pub(crate) fn stop(self) {
        self.session.stop();
    }
}

/// Clears this ceremony's prompt when it ends, including when its thread fails to start.
struct CeremonyGuard(Arc<App>, Arc<AtomicBool>);

impl Drop for CeremonyGuard {
    fn drop(&mut self) {
        self.0.pin_ceremony.lock().unwrap().take_if(|s| Arc::ptr_eq(&s.cancelled, &self.1));
    }
}

impl App {
    /// The song title to show on a speaker now (see [`title_to_send`]):
    /// from Windows' current media session, when it is a music app's and
    /// plays, and no other app's sound plays too.
    pub fn title_now(&self) -> Option<crate::now_playing::NowPlaying> {
        title_to_send(&title_sources(), &self.sounding_apps.lock().unwrap())
    }

    /// The app with the settings saved in `config.json`.
    pub fn new(
        config: AppConfig,
        discovery: Option<Arc<DiscoveryState>>,
        airplay_discovery: Option<Arc<AirPlayDiscoveryState>>,
    ) -> Arc<Self> {
        Self::with_settings(config, UserConfig::load(), discovery, airplay_discovery)
    }

    /// An app for tests: no discovery, and default settings that save
    /// nowhere (never the developer's own `config.json`).
    #[cfg(test)]
    pub(crate) fn for_tests() -> Arc<Self> {
        Self::with_settings(Self::test_config(), UserConfig::in_memory(), None, None)
    }

    /// The configuration [`App::for_tests`] runs with.
    #[cfg(test)]
    pub(crate) fn test_config() -> AppConfig {
        AppConfig {
            advertise_ip: Some("192.168.1.10".parse().unwrap()),
            bind: "0.0.0.0:8080".parse().unwrap(),
            initial_buffer_ms: 0,
            silence_packets_threshold: 0,
            no_silence_injection: false,
            no_discovery: true,
            web_enabled: false,
            ssdp_iface: None,
        }
    }

    /// The app with `user_config` as its settings.
    pub(crate) fn with_settings(
        config: AppConfig,
        user_config: UserConfig,
        discovery: Option<Arc<DiscoveryState>>,
        airplay_discovery: Option<Arc<AirPlayDiscoveryState>>,
    ) -> Arc<Self> {
        let web_initial = config.web_enabled || user_config.phone_enabled;
        let access = WebAccess::new();
        if user_config.scripts_enabled {
            access.set_scripts_key(user_config.scripts_key.as_deref().and_then(WebSecret::parse));
        }
        // Settings from earlier versions, once: the extra delay they gave
        // every Sendspin speaker becomes each paired speaker's own (see
        // `delay::upgrade`), and the Music Assistant input, which ran only
        // while Sendspin was on as well, stays on only if Sendspin was.
        let mut user_config = user_config;
        let delays_moved = user_config.upgrade_delays();
        let input_turned_off = user_config.sendspin.upgrade_source_switch();
        if delays_moved || input_turned_off {
            user_config.save();
        }
        // Seed last_speaker_id from the settings so that, even before any user
        // action, code that asks "what was the last speaker?" gets the
        // persisted answer.
        let last_speaker_id = Mutex::new(user_config.last_speaker_id.clone());
        let ma_level = user_config.ma_level.unwrap_or(100).min(100);
        let eq = EqControl::new(WIRE_SAMPLE_RATE);
        if user_config.equalizer.enabled {
            eq.request(user_config.equalizer.clone());
        }
        let session: SharedSession = Arc::new(Mutex::new(None));
        let vsync = Arc::new(VolumeSync::new());
        let windows_volume = Arc::new(WindowsSlider::default());
        let speaker_control = SpeakerControl::new(session.clone(), vsync.clone(), Arc::clone(&windows_volume));
        // One flag for "streaming enabled", read by the stream gate as it is.
        let streaming_enabled = Arc::new(AtomicBool::new(true));
        // The gate admits this PC in both privacy modes and while streaming
        // is disabled: any address this host owns, at the time it asks.
        let stream_gate =
            StreamGate::new(user_config.privacy_mode, streaming_enabled.clone(), is_local_address);
        let catch_up_step = Arc::new(AtomicU32::new(4));
        let app = Arc::new(Self {
            config,
            discovery,
            airplay_discovery,
            typed_airplay_passwords: Mutex::new(std::collections::HashMap::new()),
            airplay_asked_password: Mutex::new(std::collections::HashSet::new()),
            session,
            last_speaker_id,
            hub: StreamHub::new(),
            vsync,
            speaker_control,
            windows_volume,
            stream_active: Arc::new(AtomicBool::new(true)),
            sound_input_lost: Mutex::new(None),
            streaming_enabled,
            web_access: access,
            delay: crate::delay::DelayEngine::new(catch_up_step.clone()),
            rate_fudge_ppm: Arc::new(AtomicI32::new(0)),
            silence_pace_ms: Arc::new(AtomicU64::new(10)),
            latency_adjust_step_frames: catch_up_step,
            eq,
            packets_published_total: Arc::new(AtomicU64::new(0)),
            idle_silence_packets: Arc::new(AtomicU64::new(0)),
            source_packet_at_ms: AtomicU64::new(0),
            source_sound_at_ms: AtomicU64::new(0),
            started_at: Instant::now(),
            session_started: Mutex::new(None),
            reconnects: AtomicU32::new(0),
            last_fault: Mutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
            quit_now: AtomicBool::new(false),
            quit_hook_finished: AtomicBool::new(false),
            on_quit: OnceLock::new(),
            update: Mutex::new(UpdateState::default()),
            rescan_in_flight: Arc::new(AtomicBool::new(false)),
            last_rescan_finished_unix: Arc::new(AtomicI64::new(0)),
            last_rescan_count: Arc::new(AtomicUsize::new(0)),
            stream_gate,
            settings_health: user_config.save_health(),
            settings_notice: Mutex::new(Some(user_config.load_notice().clone()).filter(|n| !n.is_empty())),
            settings_retry_at: Mutex::new(None),
            settings_retrying: Arc::new(AtomicBool::new(false)),
            user_config: crate::user_config::SettingsCell::new(user_config),
            delay_shortcuts: Mutex::default(),
            last_error: Mutex::new(None),
            connecting: Mutex::new(None),
            connect_detail: Mutex::new(None),
            user_picks: AtomicU64::new(0),
            player_wish: Mutex::new(None),
            pin_ceremony: Mutex::new(None),
            sendspin: crate::sendspin_app::SendspinAppState {
                ma_level: Arc::new(std::sync::atomic::AtomicU8::new(ma_level)),
                ..Default::default()
            },
            manual_resolved: Mutex::new(std::collections::HashMap::new()),
            discovery_scope: Mutex::new(Resolved::All),
            adapters_cache: Mutex::new(Vec::new()),
            mdns: Mutex::new(None),
            scope_apply: Mutex::new(()),
            discovery_error: Mutex::new(None),
            command_sink: OnceLock::new(),
            engine_port: OnceLock::new(),
            engine_connects: Mutex::new(EngineConnects::default()),
            engine_connect_ended: Condvar::new(),
            test_sound: Mutex::new(None),
            capture_tap: Default::default(),
            refused: Mutex::new(Vec::new()),
            allowed: Mutex::new(Vec::new()),
            transport_change: Mutex::new(None),
            topology: Mutex::new(None),
            signals: Default::default(),
            sounding_apps: Mutex::default(),
            last_ended: Mutex::new(None),
            device_uses: Arc::default(),
        });
        if web_initial {
            if let Err(e) = app.set_web_ui_enabled(true) {
                app.record_error(format!("Couldn't turn the web UI on: {}", e));
            }
        }
        app.delay_watch_cut_outs();
        app.delay.catch_up().set_at_once(app.user_config.lock().unwrap().delay_at_once);
        app
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
            Some((msg, when)) if when.elapsed() < ERROR_BANNER_TTL => Some(msg.clone()),
            _ => None,
        }
    }

    /// Clear the current error (user dismissed the banner).
    pub fn dismiss_error(&self) {
        *self.last_error.lock().unwrap() = None;
    }

    /// What the window says about the settings: why they can't be saved,
    /// for as long as that lasts, and what loading them found, until
    /// dismissed.
    pub fn settings_attention(&self) -> (Option<String>, Option<String>) {
        let problem = self.settings_health.problem().map(|p| p.describe());
        let notice = self.settings_notice.lock().unwrap().clone();
        // Speakers by the names the app has for them now (discovery may
        // find one a moment after the start).
        (problem, notice.and_then(|n| n.describe(&|id| self.speaker_name(id))))
    }

    /// While the last save failed (not while the file is left alone on
    /// purpose), save again if the last try was [`SETTINGS_RETRY`] ago or
    /// more, so a passing fault (a virus scanner holding the file, a share
    /// that was offline) heals by itself and the window's strip goes. The
    /// window calls this while it is open; without one, every later change
    /// saves anyway. The save runs on a thread of its own, so the window
    /// never waits for the disk (nor for the brief retries of a refused
    /// rename); its handle, when one was started.
    pub fn retry_settings_save_if_due(self: &Arc<Self>, now: Instant) -> Option<std::thread::JoinHandle<()>> {
        if !matches!(self.settings_health.problem(), Some(crate::user_config::SaveProblem::Failed { .. })) {
            return None;
        }
        {
            let mut last = self.settings_retry_at.lock().unwrap();
            if last.is_some_and(|t| now.saturating_duration_since(t) < SETTINGS_RETRY) {
                return None;
            }
            if self.settings_retrying.swap(true, Ordering::SeqCst) {
                return None;
            }
            *last = Some(now);
        }
        let app = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("settings-retry".into()).spawn(move || {
            app.user_config.lock().unwrap().save();
            app.settings_retrying.store(false, Ordering::SeqCst);
        });
        match spawned {
            Ok(handle) => Some(handle),
            Err(e) => {
                debug!("settings: couldn't start the retry ({e}); the next change saves");
                self.settings_retrying.store(false, Ordering::SeqCst);
                None
            }
        }
    }

    /// Whether settings saves aren't reaching `config.json` now (the last
    /// one failed, or the file is left alone this run).
    pub(crate) fn settings_unsaved(&self) -> bool {
        self.settings_health.problem().is_some()
    }

    /// The person dismissed what loading the settings found.
    pub fn dismiss_settings_notice(&self) {
        *self.settings_notice.lock().unwrap() = None;
    }

    /// Drop a "Still connecting to …" notice once that connect has ended:
    /// it no longer says anything true.
    fn clear_connecting_notice(&self) {
        let mut le = self.last_error.lock().unwrap();
        if le.as_ref().is_some_and(|(m, _)| m.starts_with(STILL_CONNECTING_PREFIX)) {
            *le = None;
        }
    }

    /// Mark the onboarding card as dismissed and persist immediately.
    pub fn dismiss_onboarding(&self) {
        let mut uc = self.user_config.lock().unwrap();
        if !uc.onboarding_dismissed {
            uc.onboarding_dismissed = true;
            uc.save();
        }
    }

    /// Whether to show the donation prompt: hidden until the stored
    /// deadline passes. Shown on first launch (no stored value).
    pub fn should_show_donation_prompt(&self) -> bool {
        let uc = self.user_config.lock().unwrap();
        // Never ask someone who hasn't got it working yet: no saved
        // speaker means they are still setting up (or it failed for
        // them), and a donation strip above the onboarding card would
        // be asking for money before delivering anything.
        if uc.last_speaker_id.is_none() {
            return false;
        }
        match uc.donation_prompt_hidden_until {
            None => true,
            Some(t) => now_unix() >= t,
        }
    }

    /// Hide the donation prompt for `days`, persisted immediately.
    pub fn snooze_donation_prompt(&self, days: u64) {
        let mut uc = self.user_config.lock().unwrap();
        uc.donation_prompt_hidden_until = Some(now_unix() + days * 24 * 60 * 60);
        uc.save();
    }

    // ---- Update check ----------------------------------------------------

    pub fn is_check_for_updates(&self) -> bool {
        self.user_config.lock().unwrap().check_for_updates
    }

    pub fn set_check_for_updates(&self, on: bool) {
        let mut uc = self.user_config.lock().unwrap();
        if uc.check_for_updates != on {
            uc.check_for_updates = on;
            uc.save();
        }
    }

    pub fn update_state(&self) -> UpdateState {
        self.update.lock().unwrap().clone()
    }

    /// The newer release the banner should offer right now, if any. Works
    /// off the cached last result so it is correct from the first frame.
    pub fn update_banner(&self) -> Option<ReleaseInfo> {
        let current = update_check::current_version()?;
        let uc = self.user_config.lock().unwrap();
        let tag = uc.update_latest_tag.as_deref()?;
        let version = update_check::banner_candidate(
            &current,
            tag,
            uc.update_skipped_tag.as_deref(),
            uc.update_banner_hidden_until,
            now_unix(),
        )?;
        Some(ReleaseInfo {
            tag: tag.to_string(),
            version,
            url: uc
                .update_latest_url
                .clone()
                .unwrap_or_else(|| update_check::RELEASES_PAGE.to_string()),
            installer_url: None,
        })
    }

    /// "Skip this version": never offer `tag` again (a later one still shows).
    pub fn skip_update(&self, tag: &str) {
        let mut uc = self.user_config.lock().unwrap();
        uc.update_skipped_tag = Some(tag.to_string());
        uc.save();
    }

    /// "Later": hide the banner for `days`.
    pub fn snooze_update_banner(&self, days: u64) {
        let mut uc = self.user_config.lock().unwrap();
        uc.update_banner_hidden_until = Some(now_unix() + days * 24 * 60 * 60);
        uc.save();
    }

    fn update_check_due(&self) -> bool {
        let uc = self.user_config.lock().unwrap();
        if !uc.check_for_updates {
            return false;
        }
        match uc.update_last_check_unix {
            None => true,
            Some(t) => now_unix().saturating_sub(t) >= update_check::CHECK_INTERVAL.as_secs(),
        }
    }

    /// One blocking check (call off the UI thread). `manual` bypasses the
    /// enable switch and the daily cadence; a dev build never checks.
    pub fn run_update_check(&self, manual: bool) {
        let finish = |outcome: UpdateOutcome| {
            let mut st = self.update.lock().unwrap();
            st.checking = false;
            st.last = Some((outcome, now_unix()));
        };
        let Some(current) = update_check::current_version() else {
            finish(UpdateOutcome::DevBuild);
            return;
        };
        if !manual && !self.is_check_for_updates() {
            self.update.lock().unwrap().checking = false;
            return;
        }
        self.update.lock().unwrap().checking = true;
        let result = update_check::fetch_latest_release();
        let now = now_unix();
        let outcome = match result {
            Ok(rel) => {
                {
                    let mut uc = self.user_config.lock().unwrap();
                    uc.update_last_check_unix = Some(now);
                    uc.update_latest_tag = Some(rel.tag.clone());
                    uc.update_latest_url = Some(rel.url.clone());
                    uc.save();
                }
                if update_check::is_newer(&rel.version, &current) {
                    log::info!(
                        "update available: {} (this is v{}) — {}",
                        rel.tag,
                        crate::display_version(),
                        rel.url
                    );
                    UpdateOutcome::Available(rel)
                } else {
                    log::info!("update check: v{} is the latest release", crate::display_version());
                    UpdateOutcome::UpToDate
                }
            }
            Err(e) => {
                // Stamp the attempt so an offline machine retries once a
                // day, not every hour; the cached latest (if any) still
                // drives the banner.
                let mut uc = self.user_config.lock().unwrap();
                uc.update_last_check_unix = Some(now);
                uc.save();
                log::warn!("update check failed: {:#}", e);
                UpdateOutcome::Failed(format!("{e:#}"))
            }
        };
        finish(outcome);
    }

    /// Sleep `d` in slices, returning false early if shutdown began.
    pub(crate) fn sleep_unless_shutdown(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        loop {
            if self.is_shutting_down() {
                return false;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            std::thread::sleep(left.min(Duration::from_millis(250)));
        }
    }

    /// Background once-a-day update check: first attempt ~20 s after
    /// launch (never on the startup path), then whenever 24 h have passed
    /// since the last attempt, re-evaluated hourly. Everything it does is
    /// gated by `check_for_updates`; a dev build is a no-op.
    pub fn spawn_update_checker(self: &Arc<Self>) {
        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-update-check".into())
            .spawn(move || {
                if !app.sleep_unless_shutdown(update_check::STARTUP_DELAY) {
                    return;
                }
                loop {
                    if app.update_check_due() {
                        app.run_update_check(false);
                    }
                    if !app.sleep_unless_shutdown(Duration::from_secs(60 * 60)) {
                        return;
                    }
                }
            })
            .ok();
    }

    /// Help menu "Check for updates now": one manual check on a worker
    /// thread. `checking` is set here so the menu reflects it immediately.
    pub fn check_for_updates_now(self: &Arc<Self>) {
        {
            let mut st = self.update.lock().unwrap();
            if st.checking {
                return;
            }
            st.checking = true;
        }
        let app = self.clone();
        let spawned = std::thread::Builder::new()
            .name("stream-to-speaker-update-check-manual".into())
            .spawn(move || app.run_update_check(true));
        if spawned.is_err() {
            // Nothing will finish the check: don't leave "Checking…" up.
            self.update.lock().unwrap().checking = false;
            self.record_error("Couldn't check for updates (no thread available). Try again in a moment.");
        }
    }

    /// Undo the onboarding dismissal (Help menu → "Show getting-
    /// started again"). Persists immediately.
    pub fn reset_onboarding(&self) {
        if self.send_command(crate::model::Surface::Settings,
            crate::model::Command::FirstRunAction(crate::model::command::FirstRunAction::ShowAgain)) {
            return;
        }
        let mut uc = self.user_config.lock().unwrap();
        if uc.onboarding_dismissed {
            uc.onboarding_dismissed = false;
            uc.save();
        }
    }

    /// Only the place engine can choose the startup route. The old transport
    /// launch planner remains callable by older integrations.
    pub fn is_auto_reconnect_on_launch(&self) -> bool {
        false
    }

    /// The stored AirPlay password for a device id, if any.
    pub fn airplay_password(&self, id: &str) -> Option<String> {
        self.user_config.lock().unwrap().airplay_passwords.get(id).cloned()
    }

    /// Store (or clear, when `pw` is empty) the AirPlay password for a
    /// device id and persist it. Under a newcomer's id (see
    /// [`crate::airplay::discovery::is_instance_id`]) nothing is stored,
    /// whatever its record says now: it is kept only for its next connect.
    pub fn set_airplay_password(&self, id: &str, pw: &str) {
        if crate::airplay::discovery::is_instance_id(id) {
            self.keep_typed_password(id, pw);
            return;
        }
        self.forget_typed_password(id);
        let mut uc = self.user_config.lock().unwrap();
        if pw.is_empty() {
            uc.remove_airplay_password(id);
        } else {
            uc.airplay_passwords.insert(id.to_string(), pw.to_string());
        }
        uc.save();
        // Written as the lock goes.
        drop(uc);
        if self.settings_unsaved() && !pw.is_empty() {
            self.record_error(
                "The password can't be saved with the settings, so you'll need to enter it again \
                 after the app restarts.",
            );
        }
    }

    /// The password dialog for `id` (`name` for its title), as it opens:
    /// whether what is typed there is kept for the speaker or used for one
    /// connection is decided now, from the record, and travels with the
    /// dialog.
    pub fn password_dialog(&self, id: &str, name: &str) -> PasswordDialog {
        let one_session = self.is_airplay_newcomer(id) || crate::airplay::discovery::is_instance_id(id);
        PasswordDialog { id: id.to_string(), name: name.to_string(), one_session }
    }

    /// Take what was typed in `dialog`, as the dialog decided when it
    /// opened (never by asking the record again: it may have changed
    /// while the dialog was open). If the entry it was opened for has gone
    /// meanwhile, nothing is taken, and the error says what to do.
    pub fn enter_airplay_password(&self, dialog: &PasswordDialog, pw: &str) -> Result<(), String> {
        let listed = self.find_manual_speaker(&dialog.id).is_some()
            || self.airplay_discovery.as_ref().and_then(|d| d.find_by_id(&dialog.id)).is_some();
        if !listed {
            return Err(format!(
                "Couldn't connect: “{}” is no longer in the speaker list, and the password you typed wasn't kept. \
                 When it's back in the list, pick it and enter the password again.",
                dialog.name
            ));
        }
        if dialog.one_session {
            self.keep_typed_password(&dialog.id, pw);
        } else {
            self.set_airplay_password(&dialog.id, pw);
            if !pw.is_empty() {
                self.typed_airplay_passwords.lock().unwrap().insert(dialog.id.clone(), TypedPassword::Saved);
            }
        }
        Ok(())
    }

    /// The window's password dialog was confirmed: take the password, then
    /// connect.
    pub fn submit_password_dialog(self: &Arc<Self>, dialog: &PasswordDialog, pw: &str) -> Result<(), String> {
        self.enter_airplay_password(dialog, pw)?;
        self.select_speaker_async(&dialog.id);
        Ok(())
    }

    /// Keep `pw` for the next connect to `id` only (an empty one clears
    /// it); never stored.
    fn keep_typed_password(&self, id: &str, pw: &str) {
        let mut typed = self.typed_airplay_passwords.lock().unwrap();
        if pw.is_empty() {
            typed.remove(id);
        } else {
            typed.insert(id.to_string(), TypedPassword::Once(pw.to_string()));
        }
    }

    /// A password typed for one connection lives until the connect it was
    /// typed for ends, used or not.
    fn forget_typed_password(&self, id: &str) {
        self.typed_airplay_passwords.lock().unwrap().remove(id);
    }

    /// The password to send, and whether it came from the dialog for this connection.
    /// A one-connection password follows its instance if an identity conflict ends.
    fn airplay_password_for(&self, renderer: &AirPlayRenderer) -> (Option<String>, bool) {
        let stable_id = renderer.stable_id();
        let typed = {
            let mut typed = self.typed_airplay_passwords.lock().unwrap();
            renderer.instance_id().and_then(|k| typed.remove(&k)).or_else(|| typed.remove(&stable_id))
        };
        if let Some(TypedPassword::Once(pw)) = typed {
            return (Some(pw), true);
        }
        let password = self.user_config.lock().unwrap().airplay_passwords.get(&stable_id).filter(|_| !renderer.is_newcomer()).cloned();
        let entered = password.is_some() && matches!(typed, Some(TypedPassword::Saved));
        (password, entered)
    }

    /// Does `id` name a discovered AirPlay receiver (or pair) that
    /// announces another speaker's identity? Asked of the record.
    pub(crate) fn is_airplay_newcomer(&self, id: &str) -> bool {
        self.airplay_discovery
            .as_ref()
            .and_then(|d| d.find_by_id(id))
            .is_some_and(|r| r.is_newcomer())
    }

    /// Whether a discovered device is password-protected (needs a
    /// password entered before it can connect).
    pub fn is_password_protected(&self, id: &str) -> bool {
        self.asked_for_password(id)
            || self
                .airplay_discovery
                .as_ref()
                .and_then(|d| d.find_by_id(id))
                .map(|r| r.password_protected)
                .unwrap_or(false)
    }

    /// Whether to forward Windows "now playing" to the speaker.
    pub fn is_forward_now_playing(&self) -> bool {
        self.user_config.lock().unwrap().forward_now_playing
    }

    // ---- Equalizer -------------------------------------------------------

    /// The saved equalizer curve.
    pub fn eq_saved(&self) -> EqSettings {
        self.user_config.lock().unwrap().equalizer.clone()
    }

    /// Apply `settings` to the running stream without saving (live
    /// preview while editing). No reconnect: the change is crossfaded in.
    pub fn eq_preview(&self, settings: &EqSettings) {
        self.eq.request(settings.clone());
    }

    /// Headroom figures of the most recently designed curve.
    pub fn eq_report(&self) -> Option<EqReport> {
        self.eq.report()
    }

    // ---- Privacy mode ---------------------------------------------------

    /// Whether privacy mode is on: `/stream.raw` served only to speakers
    /// with a live session, plus this machine itself.
    pub fn is_privacy_mode(&self) -> bool {
        self.stream_gate.privacy()
    }

    /// Persist the privacy-mode preference and apply it. Turning it ON
    /// also cuts off any `/stream.raw` listener the gate wouldn't admit
    /// now — otherwise an established connection would keep receiving
    /// audio indefinitely.
    pub fn set_privacy_mode(&self, on: bool) {
        {
            let mut uc = self.user_config.lock().unwrap();
            if uc.privacy_mode == on {
                return;
            }
            uc.privacy_mode = on;
            uc.save();
        }
        self.stream_gate.set_privacy(on);
        info!("privacy mode {}", if on { "enabled" } else { "disabled" });
        if on {
            self.prune_stream_clients();
        }
    }

    /// Whether `peer` belongs to the active speaker (or its group). The
    /// HTTP server never lets other clients crowd these out.
    pub fn is_speaker_peer(&self, peer: IpAddr) -> bool {
        self.stream_gate.is_granted(peer)
    }

    /// The stream gate for `/stream.raw`, called per request (and once a
    /// second per open stream) from the HTTP threads, which tell the
    /// refused device and the log why. Touches only the gate's own brief
    /// lock — never `session`, which other threads hold across
    /// multi-second SOAP calls — so a stalled speaker can't park the HTTP
    /// accept loop. A privacy-mode refusal is surfaced to the user,
    /// rate-limited per address, naming the device when discovery knows
    /// it. One while streaming is disabled is only logged: closing the
    /// stream is what Disable is for.
    pub fn check_stream_client(&self, peer: IpAddr) -> Result<(), Refusal> {
        let verdict = self.stream_gate.check(peer);
        if verdict == Err(Refusal::NotGranted) {
            self.note_refused(peer);
        }
        if verdict == Err(Refusal::NotGranted) && self.stream_gate.note_refusal(peer) {
            self.record_error(format!(
                "Refused a connection to the audio stream from {}: only the picked speaker \
                 may listen. A refused device can be allowed on the speaker's page or the Lab page.",
                self.describe_peer(peer)
            ));
        }
        verdict
    }

    /// "Kitchen (192.168.1.50)" when discovery knows the address, else
    /// the bare address. Brief locks only.
    fn describe_peer(&self, peer: IpAddr) -> String {
        let peer = canonical_ip(peer);
        let upnp = self.discovery.as_ref().and_then(|d| {
            d.renderers()
                .into_iter()
                .find(|r| r.stream_peers().iter().any(|ip| canonical_ip(*ip) == peer))
                .map(|r| r.friendly_name)
        });
        let name = upnp.or_else(|| {
            self.airplay_discovery.as_ref().and_then(|d| {
                d.renderers()
                    .into_iter()
                    .find(|r| canonical_ip(r.ip) == peer)
                    .map(|r| r.friendly_name)
            })
        });
        match name {
            Some(n) => format!("{} ({})", n, peer),
            None => peer.to_string(),
        }
    }

    /// Cut off every `/stream.raw` listener the gate no longer admits —
    /// after a speaker switch, Disable, Forget, or privacy turning on.
    /// Applies in both privacy modes: with streaming disabled the gate
    /// admits no device but this PC. The allow-set is snapshotted first,
    /// so the hub's subscriber lock (taken by the audio thread per packet)
    /// is held only for a pure predicate, never while waiting on anything.
    pub fn prune_stream_clients(&self) {
        let snapshot = self.stream_gate.snapshot();
        let cut = self.hub.disconnect_clients(|ip| !snapshot.allows(ip));
        if cut > 0 {
            info!(
                "stream: disconnected {} listener(s) no longer allowed ({})",
                cut,
                if !snapshot.streaming() { "streaming is disabled" } else { "privacy mode" }
            );
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
    // Manually added speakers
    // -------------------------------------------------------------------

    /// Speakers the user added by address, in the order added.
    pub fn manual_speakers(&self) -> Vec<ManualSpeaker> {
        self.user_config.lock().unwrap().manual_speakers.clone()
    }

    /// The manual entry with this id, if any.
    pub fn find_manual_speaker(&self, id: &str) -> Option<ManualSpeaker> {
        if !manual_speakers::is_manual_id(id) {
            return None;
        }
        self.user_config
            .lock()
            .unwrap()
            .manual_speakers
            .iter()
            .find(|m| m.id() == id)
            .cloned()
    }

    /// Validate and persist a new manual speaker (the add dialog's
    /// fields). Returns its id, or a user-facing reason it was refused.
    pub fn add_manual_speaker(
        &self,
        kind: ManualKind,
        name: &str,
        host: &str,
        port: &str,
    ) -> Result<String, String> {
        let mut uc = self.user_config.lock().unwrap();
        let entry = manual_speakers::validate(kind, name, host, port, &uc.manual_speakers)?;
        let id = entry.id();
        info!("manual speaker added: {} ({})", entry.display_name(), id);
        uc.manual_speakers.push(entry);
        uc.save();
        Ok(id)
    }

    /// Remove a manual speaker: disconnect it if it's the active one and
    /// drop what was stored under its id.
    /// The teardown runs on a worker thread: it is network I/O and this is
    /// called from the GUI. A connect in flight meanwhile may be to this
    /// speaker: if it binds it, that is undone once it ends.
    pub fn remove_manual_speaker(self: &Arc<Self>, id: &str) {
        let stopped = {
            let mut guard = self.session.lock().unwrap();
            if guard.as_ref().map_or(false, |s| s.stable_id() == id) {
                guard.take().map(|s| self.releasing(s, None))
            } else {
                None
            }
        };
        if let Some(s) = stopped {
            let cell = Arc::new(Mutex::new(Some(s)));
            let app = self.clone();
            let worker_cell = cell.clone();
            let spawned = std::thread::Builder::new()
                .name("stream-to-speaker-stop".into())
                .spawn(move || {
                    if let Some(s) = worker_cell.lock().unwrap().take() {
                        s.stop();
                    }
                    // Its privacy grant went with it.
                    app.prune_stream_clients();
                });
            if spawned.is_err() {
                if let Some(s) = cell.lock().unwrap().take() {
                    s.stop();
                }
                self.prune_stream_clients();
            }
        }
        {
            let mut last = self.last_speaker_id.lock().unwrap();
            if last.as_deref() == Some(id) {
                *last = None;
            }
        }
        self.manual_resolved.lock().unwrap().remove(id);
        let mut uc = self.user_config.lock().unwrap();
        let before = uc.manual_speakers.len();
        uc.manual_speakers.retain(|m| m.id() != id);
        if uc.manual_speakers.len() == before {
            return;
        }
        if uc.last_speaker_id.as_deref() == Some(id) {
            uc.last_speaker_id = None;
        }
        // Everything kept for it goes with it: its password and pairing,
        // sealed copies this account can't open included (a save would
        // keep those), and its settings, so the same address added again
        // is a new speaker.
        uc.remove_airplay_password(id);
        uc.remove_airplay_pairing(id);
        uc.speaker_settings.remove(id);
        uc.save();
        drop(uc);
        info!("manual speaker removed: {}", id);
        // Gone from the list first, then: a connect that starts from now on
        // can't find it, and one already under way may have, and is watched.
        if self.connecting_to().is_some() {
            self.undo_connect_once_done(id.to_string());
        }
    }

    /// Once the connect in flight has ended: if it bound the removed
    /// speaker `id` (and saved it as the last one), stop it and forget it.
    /// Off the caller's thread; gives up after a minute.
    fn undo_connect_once_done(self: &Arc<Self>, id: String) {
        let app = self.clone();
        let spawned = std::thread::Builder::new().name("stream-to-speaker-removed".into()).spawn(move || {
            let end = Instant::now() + Duration::from_secs(60);
            while app.connecting_to().is_some() && Instant::now() < end {
                if !app.sleep_unless_shutdown(Duration::from_millis(50)) {
                    return;
                }
            }
            let stopped = {
                let mut guard = app.session.lock().unwrap();
                if guard.as_ref().is_some_and(|s| s.stable_id() == id) {
                    guard.take().map(|s| app.releasing(s, None))
                } else {
                    None
                }
            };
            {
                let mut last = app.last_speaker_id.lock().unwrap();
                if last.as_deref() == Some(id.as_str()) {
                    *last = None;
                }
            }
            {
                let mut uc = app.user_config.lock().unwrap();
                if uc.last_speaker_id.as_deref() == Some(id.as_str()) {
                    uc.last_speaker_id = None;
                    uc.save();
                }
            }
            if let Some(s) = stopped {
                info!("stopped {:?}: it was removed while connecting", id);
                s.stop();
                app.prune_stream_clients();
            }
        });
        if spawned.is_err() {
            warn!("couldn't watch the connect under way: if it was to the removed speaker, it stays connected");
        }
    }

    /// Display name for any speaker id: discovered UPnP, discovered
    /// AirPlay, or manual.
    pub(crate) fn speaker_name(&self, id: &str) -> Option<String> {
        if let Some(name) = self.speaker_settings(id).show_as.filter(|n| !n.trim().is_empty()) {
            return Some(name);
        }
        if let Some(m) = self.find_manual_speaker(id) {
            return Some(m.display_name());
        }
        self.discovery
            .as_ref()
            .and_then(|d| d.find_by_id(id))
            .map(|r| r.display_name())
            .or_else(|| {
                self.airplay_discovery
                    .as_ref()
                    .and_then(|d| d.find_by_id(id))
                    .map(|r| r.friendly_name)
            })
            .or_else(|| self.sendspin_name_for(id))
    }

    /// The AirPlay receiver record for an id: discovered, or the last
    /// probe of a manual entry.
    pub(crate) fn find_airplay_renderer(&self, id: &str) -> Option<AirPlayRenderer> {
        if manual_speakers::is_manual_id(id) {
            return self.manual_resolved.lock().unwrap().get(id).cloned();
        }
        self.airplay_discovery.as_ref().and_then(|d| d.find_by_id(id))
    }

    // -------------------------------------------------------------------
    // Speaker view
    // -------------------------------------------------------------------

    /// Snapshot of discovered speakers and which one (if any) is active.
    /// Merges the UPnP and AirPlay discovery lists; the GUI sees a
    /// single sorted list with one row per speaker regardless of which
    /// protocol it speaks.
    pub fn speaker_view(&self) -> SpeakerView {
        self.speaker_view_opts(false)
    }

    /// [`speaker_view`] with the user-hidden speakers included (flagged)
    /// when `include_hidden`; without it they're left out unless active.
    ///
    /// [`speaker_view`]: App::speaker_view
    pub fn speaker_view_opts(&self, include_hidden: bool) -> SpeakerView {
        let (active_id, active_upnp) = {
            let guard = self.session.lock().unwrap();
            let id = guard.as_ref().map(|s| s.stable_id());
            let upnp = match guard.as_ref() {
                Some(ActiveSession::Upnp(s)) => Some(s.renderer.clone()),
                _ => None,
            };
            (id, upnp)
        };
        let mut speakers: Vec<SpeakerInfo> = Vec::new();

        // Track the IPs that have a non-AirPlay (UPnP) entry, so we can
        // warn on the AirPlay twin that it'll have higher latency (the
        // AirPlay path buffers ~1–2 s; the UPnP path is lower-latency).
        let mut upnp_ips: std::collections::HashSet<String> = std::collections::HashSet::new();

        if let Some(d) = self.discovery.as_ref() {
            for r in d.renderers() {
                let id = r.stable_id();
                let active = active_id.as_deref() == Some(id.as_str())
                    || active_upnp.as_ref().map(|a| a.same_device_as(&r)).unwrap_or(false);
                upnp_ips.insert(r.ip.to_string());
                // Sonos group coordinators present as one row for the
                // whole group ("Living Room + Kitchen"); the grouped
                // members themselves are filtered out at discovery time
                // because streaming to one would silently ungroup it.
                let note = r.is_group().then(|| {
                    let mut all = vec![r
                        .zone_name
                        .clone()
                        .unwrap_or_else(|| r.friendly_name.clone())];
                    all.extend(r.group_members.iter().cloned());
                    format!(
                        "Sonos group: {}. Audio plays on every speaker in the group and the \
                         volume slider moves the whole group. To stream to a single speaker, \
                         ungroup it in the Sonos app first.",
                        all.join(" + ")
                    )
                });
                speakers.push(SpeakerInfo {
                    id,
                    friendly_name: r.display_name(),
                    ip: r.ip.to_string(),
                    active,
                    note,
                    manual: false,
                    hidden: false,
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
                if let Some(pair) = &r.pair {
                    let members: Vec<&str> = pair.members.iter().map(|m| m.friendly_name.as_str()).collect();
                    let (name, note) = if pair.is_complete() {
                        (
                            format!("{} (AirPlay 2 stereo pair)", r.friendly_name),
                            format!(
                                "HomePod stereo pair: {}. Both HomePods play, each its own channel, and the \
                                 volume slider moves both.",
                                members.join(" + ")
                            ),
                        )
                    } else {
                        (
                            format!(
                                "{} (stereo pair, {} of {} present)",
                                r.friendly_name,
                                pair.members.len(),
                                pair.expected
                            ),
                            "Waiting for the other HomePod of this stereo pair to come online. The pair \
                             can only be played with both HomePods present."
                                .to_string(),
                        )
                    };
                    speakers.push(SpeakerInfo {
                        id,
                        friendly_name: name,
                        ip: r.ip.to_string(),
                        active,
                        note: Some(note),
                        manual: false,
                        hidden: false,
                    });
                    continue;
                }
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
                    manual: false,
                    hidden: false,
                });
            }
        }
        speakers.extend(self.sendspin_speakers(active_id.as_deref()));
        for m in self.manual_speakers() {
            let id = m.id();
            let active = active_id.as_deref() == Some(id.as_str());
            let (proto, how) = match m.kind {
                ManualKind::AirPlay => ("AirPlay", "AirPlay receiver"),
                ManualKind::Upnp => ("UPnP", "UPnP device description"),
            };
            speakers.push(SpeakerInfo {
                friendly_name: format!("{} ({}, added manually)", m.display_name(), proto),
                ip: m.address_label(),
                active,
                note: Some(format!(
                    "Added by address ({} at {}). It's always listed, even when discovery \
                     doesn't see it; connecting tells you if it can't be reached.",
                    how, m.host
                )),
                manual: true,
                hidden: false,
                id,
            });
        }
        {
            let uc = self.user_config.lock().unwrap();
            for sp in speakers.iter_mut() {
                sp.hidden = uc.speaker_settings.get(&sp.id).map_or(false, |s| s.hidden);
            }
        }
        if !include_hidden {
            speakers.retain(|sp| !sp.hidden || sp.active);
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

    /// The bound session has dropped: it stays bound until the engine
    /// lets it go or connects again.
    pub fn selected_session_dead(&self) -> bool {
        self.session.lock().unwrap().as_ref().is_some_and(|s| s.is_dead())
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
    /// ceremony launched. Automatic callers (the engine's tries, the
    /// ceremony's own post-pair reconnect) use
    /// [`select_speaker_async_opts`] with `interactive = false` so an
    /// unattended retry can never pop a PIN prompt on the user's TV.
    ///
    /// [`select_speaker_async_opts`]: App::select_speaker_async_opts
    pub fn select_speaker_async(self: &Arc<Self>, id: &str) {
        // The window's click: a person in the app's own window.
        if self.send_command(crate::model::Surface::Panel, crate::model::Command::Switch { place: place_of(id), way: None }) {
            return;
        }
        self.note_user_pick();
        self.reconnects.store(0, Ordering::Relaxed);
        self.select_speaker_async_opts(id, true, false);
    }

    /// Record that the user picked a speaker (see `user_picks`).
    pub(crate) fn note_user_pick(&self) -> u64 {
        self.user_picks.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }

    /// How many times the person chose where the sound plays.
    pub(crate) fn user_pick_count(&self) -> u64 {
        self.user_picks.load(Ordering::SeqCst)
    }

    /// Non-interactive [`select_speaker_async`] for automatic callers
    /// (launch reconnect to a speaker added by address).
    ///
    /// [`select_speaker_async`]: App::select_speaker_async
    pub fn connect_in_background(self: &Arc<Self>, id: &str) {
        // With the engine, a launch reconnect is its own decision.
        if self.command_sink.get().is_some() {
            return;
        }
        self.select_speaker_async_opts(id, false, false);
    }

    /// `only_if_idle`: give up unless nothing is bound either — decided
    /// under the same `connecting` lock every select claims, so an
    /// automatic connect can never replace a speaker the user picked.
    fn select_speaker_async_opts(self: &Arc<Self>, id: &str, interactive: bool, only_if_idle: bool) {
        self.select_speaker_async_full(id, interactive, only_if_idle, None);
    }

    /// Automatic, non-interactive connect that gives way to the user:
    /// skipped when they picked a speaker since [`user_pick_count`]
    /// returned `picks_seen`, decided under the `connecting` lock.
    ///
    /// [`user_pick_count`]: App::user_pick_count
    pub(crate) fn connect_unless_user_picked(self: &Arc<Self>, id: &str, picks_seen: u64) {
        // With the engine, a launch reconnect is its own decision.
        if self.command_sink.get().is_some() {
            return;
        }
        self.select_speaker_async_full(id, false, false, Some(picks_seen));
    }

    fn select_speaker_async_full(
        self: &Arc<Self>,
        id: &str,
        interactive: bool,
        only_if_idle: bool,
        unless_picked_since: Option<u64>,
    ) {
        {
            let mut guard = self.connecting.lock().unwrap();
            if only_if_idle && guard.is_none() && self.session.lock().unwrap().is_some() {
                return;
            }
            if unless_picked_since.is_some_and(|seen| seen != self.user_pick_count()) {
                debug!("select of {} skipped — the user picked a speaker", id);
                return;
            }
            if let Some(name) = guard.as_ref() {
                if interactive {
                    self.record_error(format!("{}{} — give it a moment.", STILL_CONNECTING_PREFIX, name));
                    self.forget_typed_password(id);
                } else {
                    debug!("select of {} skipped — still connecting to {}", id, name);
                }
                return;
            }
            // Resolve a display name best-effort for the banner.
            let name = self.speaker_name(id).unwrap_or_else(|| id.to_string());
            *guard = Some(name);
        }
        let app = self.clone();
        let requested = id;
        let id = id.to_string();
        if std::thread::Builder::new()
            .name("stream-to-speaker-connect".into())
            .spawn(move || {
                // A click is the user's choice; other background connects
                // (reconnects, auto-connect) only bind while streaming is
                // still enabled.
                let result = app.select_speaker_opts(&id, interactive, interactive);
                app.forget_typed_password(&id);
                *app.connecting.lock().unwrap() = None;
                app.clear_connecting_notice();
                if let Err(e) = result {
                    let msg = format!("Couldn't connect to speaker: {}", e);
                    app.record_fault(&msg);
                    app.record_error(msg);
                }
            })
            .is_err()
        {
            *self.connecting.lock().unwrap() = None;
            self.forget_typed_password(requested);
            self.record_error("Couldn't start connecting (no thread available).");
        }
    }

    /// Background forwarder: when enabled, pushes the OS "now playing"
    /// (title/artist/album) to the bound speaker as it changes, so the
    /// track shows on the speaker's display / app: only a music app's
    /// title, while it plays and no other app's sound plays too (see
    /// [`title_to_send`]). RAOP only (AP2/UPnP sessions are skipped).
    /// Entirely best-effort — metadata is non-fatal on every receiver, so
    /// a failure or an unsupported OS never touches the audio path.
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

                    if !app.song_titles_for_playing() {
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

                    let np = app.title_now();
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
        self.select_speaker_from(crate::model::Surface::Api, id)
    }

    /// [`Self::select_speaker`] from `from` (the web API, the terminal
    /// prompt): with the engine running, the pick goes to it, after the same
    /// guard as before.
    pub fn select_speaker_from(self: &Arc<Self>, from: crate::model::Surface, id: &str) -> Result<(), String> {
        // A receiver announcing another speaker's identity plays only when
        // a person picks it in the window, never through this entry: the
        // web API (which scripts can drive), the launch connect, a resync
        // (which leaves the session playing) or Enable.
        if self.is_airplay_newcomer(id) {
            // Its name says what it is ("<speaker> — another receiver with
            // the same ID: …").
            return Err(format!(
                "To play to “{}”, pick it in the Stream To Speaker window.",
                self.speaker_name(id).unwrap_or_else(|| id.to_string())
            ));
        }
        if self.command_sink.get().is_some() {
            return self.play_and_wait(from, id, None);
        }
        self.select_speaker_now(id)
    }

    /// Ask the engine to play on `id` and wait (on this thread, which does
    /// no network I/O meanwhile) until the connect it starts has ended:
    /// `Ok` once the speaker plays, else the words saying why not.
    fn play_and_wait(self: &Arc<Self>, from: crate::model::Surface, id: &str, picks_seen: Option<u64>) -> Result<(), String> {
        let speaker = crate::engine::place_driver::speaker_for_pick(self, id);
        // Only a generation that had already ended can be ignored.
        let finished_before = self.engine_connects.lock().unwrap().by_id.get(speaker.as_str())
            .and_then(|(gen, result)| result.as_ref().map(|_| *gen));
        let Some(sink) = self.command_sink.get() else { return Err("Stream To Speaker isn't running.".into()) };
        let command = match picks_seen {
            Some(count) => crate::model::Command::PlayWhenListed { speaker: speaker.clone(), picks_seen: count },
            None => crate::model::Command::Switch { place: place_of(id), way: None },
        };
        let outcome = sink(from, command).wait(Duration::from_secs(10));
        let Some(outcome) = outcome else {
            return Err(format!("{} didn't answer. Try again in a moment.", PRODUCT_NAME));
        };
        let latest = || self.engine_view().unwrap_or_else(|| outcome.snapshot.clone());
        if let Err(why) = &outcome.result {
            let view = latest();
            return Err(crate::model::view::refusal_text(view.lang, why, &view));
        }
        // The connect it started, or the one already under way to it.
        let end = Instant::now() + CONNECT_BUDGET + Duration::from_secs(15);
        let mut connects = self.engine_connects.lock().unwrap();
        loop {
            match connects.by_id.get(speaker.as_str()).cloned() {
                Some((gen, Some(result))) if Some(gen) != finished_before => return result,
                Some((_, None)) => {}
                // Nothing new to wait for: it plays there already, or the
                // engine had no reason to connect.
                _ => {
                    let view = latest();
                    return if view.scene.situation.place_speaker() == Some(&speaker) {
                        Ok(())
                    } else if from == crate::model::Surface::CommandLine {
                        Err(view.cli_status.clone())
                    } else {
                        Err(view.status.clone())
                    };
                }
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "{} is still connecting to {}.",
                    PRODUCT_NAME,
                    latest().scene.speaker_name(&speaker)
                ));
            }
            connects = self.engine_connect_ended.wait_timeout(connects, left).unwrap().0;
        }
    }

    /// The engine started connect `gen` to `id`.
    pub(crate) fn engine_connect_started(&self, id: &str, gen: u64) {
        self.engine_connects.lock().unwrap().by_id.insert(id.to_string(), (gen, None));
    }

    /// The engine's connect `gen` to `id` ended.
    pub(crate) fn engine_connect_ended(&self, id: &str, gen: u64, result: Result<(), String>) {
        let mut connects = self.engine_connects.lock().unwrap();
        if connects.by_id.get(id).is_some_and(|(g, _)| *g == gen) {
            connects.by_id.insert(id.to_string(), (gen, Some(result)));
            self.engine_connect_ended.notify_all();
        }
    }

    /// Connect `id` now, on this thread, as a person's choice that can't
    /// answer a code prompt (today's path, without the engine).
    pub(crate) fn select_speaker_now(self: &Arc<Self>, id: &str) -> Result<(), String> {
        if self.is_airplay_newcomer(id) {
            return Err(format!(
                "To play to “{}”, pick it in the Stream To Speaker window.",
                self.speaker_name(id).unwrap_or_else(|| id.to_string())
            ));
        }
        self.note_user_pick();
        // Claim the same `connecting` slot the background connects use, so
        // the two can't run at once and auto-connect sees this one.
        {
            let mut guard = self.connecting.lock().unwrap();
            if let Some(name) = guard.as_ref() {
                return Err(format!("Still connecting to {} — try again in a moment.", name));
            }
            *guard = Some(self.speaker_name(id).unwrap_or_else(|| id.to_string()));
        }
        struct Release<'a>(&'a Mutex<Option<String>>);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                *self.0.lock().unwrap() = None;
            }
        }
        let _release = Release(&self.connecting);
        self.select_speaker_opts(id, false, true)
    }

    /// Connect speaker `id`, which plays, again with what its settings hold
    /// now (a delay change), on this thread: not the person's pick, never a
    /// PIN prompt, and only while streaming is enabled. `Err(None)` while
    /// another connect is under way.
    pub(crate) fn connect_again(self: &Arc<Self>, id: &str) -> Result<(), Option<String>> {
        {
            let mut guard = self.connecting.lock().unwrap();
            if guard.is_some() {
                return Err(None);
            }
            *guard = Some(self.speaker_name(id).unwrap_or_else(|| id.to_string()));
        }
        let result = self.select_speaker_opts(id, false, false);
        *self.connecting.lock().unwrap() = None;
        self.clear_connecting_notice();
        result.map_err(Some)
    }

    /// `user_choice`: the user asked for this speaker now, which (re-)enables
    /// streaming up front. Either way the new session binds only if
    /// streaming is still enabled when it is up — a Disable pressed while
    /// it was connecting wins, and the fresh session is stopped instead.
    fn select_speaker_opts(
        self: &Arc<Self>,
        id: &str,
        interactive: bool,
        user_choice: bool,
    ) -> Result<(), String> {
        // A receiver that announces another speaker's identity is played
        // only when a person picks it, one session at a time: no
        // reconnect, no automatic connect.
        if !user_choice && self.is_airplay_newcomer(id) {
            return Err(format!(
                "Not connecting to {} by itself: it announces the same identity as another speaker. Pick it in the \
                 speaker list to play to it.",
                self.speaker_name(id).unwrap_or_else(|| id.to_string())
            ));
        }
        if user_choice {
            self.streaming_enabled.store(true, Ordering::Release);
        }
        // However the connect ends, the delay follows the speaker that plays
        // then: a change made while it connected applies (see `delay`).
        struct DelayFollows<'a>(&'a Arc<App>);
        impl Drop for DelayFollows<'_> {
            fn drop(&mut self) {
                self.0.delay_session_bound();
                self.0.tone_follows_speaker();
            }
        }
        let _delay_follows = DelayFollows(self);
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
        // Compare devices, not id strings: a UPnP speaker's id switches
        // between its UDN and its address as identity conflicts appear and
        // clear, and stopping the "old" session after starting the new one
        // would send Stop to the very speaker we just started.
        let target = if id.starts_with("airplay:") {
            None
        } else {
            self.discovery.as_ref().and_then(|d| d.find_by_id(id))
        };
        let replacing = {
            let mut guard = self.session.lock().unwrap();
            if crate::engine::cancel::cancelled() {
                return Err(SUPERSEDED.into());
            }
            if guard.as_ref().is_some_and(|s| s.is_device(id, target.as_ref())) {
                take_for_connect(&mut guard, self.session_number(), crate::engine::cancel::binding().as_deref()).map(|s| self.releasing(s, None))
            } else { None }
        };
        if let Some(old) = replacing { old.stop(); }
        // The session's stable id can legitimately differ from the
        // requested one: selecting a Sonos that joined a group since the
        // last scan streams to its group coordinator instead (see
        // start_upnp). Remember the id we actually bound so reconnect /
        // active-row highlighting track reality.
        let manual = self.find_manual_speaker(id);
        let asking = crate::engine::cancel::asking_speaker();
        let started = if id == crate::sendspin_app::MA_OUTPUT_ID {
            let device = self.wait_for_release(id, None)?;
            (self.start_ma_output()?, device)
        } else if crate::sendspin_app::is_sendspin_id(id) {
            let device = self.wait_for_release(id, None)?;
            (self.start_sendspin_player(id, interactive)?, device)
        } else if let Some(m) = manual.as_ref().filter(|m| m.kind == ManualKind::Upnp) {
            self.start_upnp_manual(m)?
        } else if id.starts_with("airplay:") {
            match self.start_airplay(id, manual.as_ref()) {
                Ok(s) => s,
                Err(SelectFailure::Msg(m)) => return Err(m),
                Err(SelectFailure::NeedsPin { id: pid, name, reason, details }) => {
                    // Keep the other paths' failures visible — the RAOP
                    // fallback's reason would otherwise vanish behind the
                    // pairing message.
                    let suffix = if details.is_empty() {
                        String::new()
                    } else {
                        format!("  (Other paths tried: {})", details)
                    };
                    // GUI speaker click: launch the one-time ceremony — the
                    // receiver will display a PIN and the modal collects
                    // it; the toast narrates what's happening. Automatic or
                    // remote callers: nobody could answer a PIN prompt, so
                    // don't make the receiver display one — the message
                    // says where to do it instead.
                    if interactive {
                        if crate::engine::cancel::binding().is_some() && pid == id {
                            self.pair_for_job(pid, name)?;
                            return self.select_speaker_opts(id, false, user_choice);
                        }
                        self.begin_pin_pairing(pid, name.clone(), None);
                    }
                    return Err(format!("{}{}", pin_pairing_message(&name, &reason, interactive), suffix));
                }
            }
        } else {
            self.start_upnp(id)?
        };
        drop(asking);
        self.bind_selected_session(started)
    }

    fn bind_selected_session(self: &Arc<Self>, (new_session, _device): (ActiveSession, DeviceUse)) -> Result<(), String> {
        let actual_id = new_session.stable_id();
        let is_newcomer = new_session.is_newcomer();

        let mut guard = self.session.lock().unwrap();
        // A newer choice superseded this connect while it ran: what it set
        // up is stopped, not used.
        loop {
            if crate::engine::cancel::cancelled() {
                drop(guard);
                info!("a newer choice superseded the connect to {}; not binding", actual_id);
                new_session.stop();
                self.prune_stream_clients();
                return Err(SUPERSEDED.to_string());
            }
            let Some(old) = take_for_connect(&mut guard, self.session_number(), crate::engine::cancel::binding().as_deref()) else { break };
            let old = self.releasing(old, None);
            drop(guard);
            match (&old.session, &new_session) {
                (ActiveSession::Upnp(before), ActiveSession::Upnp(after)) if before.renderer.same_device_as(&after.renderer) => {
                    // The coordinator is already playing the new stream.
                    unsubscribe_all(before);
                }
                _ => old.stop(),
            }
            guard = self.session.lock().unwrap();
        }
        // Checked under the session lock: Disable flips the flag before it
        // takes the session, so either it sees ours or we see its flag.
        if !self.streaming_enabled.load(Ordering::Acquire) {
            drop(guard);
            info!("streaming was disabled while connecting to {}; not binding", actual_id);
            new_session.stop();
            self.prune_stream_clients();
            return Ok(());
        }
        // Checked under the session lock: turning Sendspin off saves the
        // setting first and then looks for a Sendspin session here, so
        // either it sees this one or this sees the setting.
        let still_allowed = match &new_session {
            ActiveSession::Sendspin(_) => self.is_sendspin_enabled(),
            // The input stops (and unselects this) when turned off.
            ActiveSession::MusicAssistant(_) => self.is_sendspin_source_enabled(),
            _ => true,
        };
        if !still_allowed {
            drop(guard);
            let msg = match &new_session {
                ActiveSession::MusicAssistant(_) => "the Music Assistant input was turned off",
                _ => "Sendspin was turned off in settings",
            };
            new_session.stop();
            return Err(msg.into());
        }
        // The level of the speaker being bound, when known now (a UPnP
        // speaker is asked below).
        let new_level = new_session.level_on_connect();
        let new_airplay = matches!(&new_session, ActiveSession::AirPlay(_) | ActiveSession::AirPlay2(_));
        if let ActiveSession::Sendspin(s) = &new_session {
            s.inherit_local_mute(self.vsync.is_muted());
        }
        let session_number = self.bind_session(&mut guard, new_session);
        // CRITICAL: drop the session-mutex guard before calling any
        // method that re-locks it. std::sync::Mutex is NOT re-entrant
        // — methods further down also call `self.session.lock()`, and
        // on the same thread that's a deadlock. Pull out what we need
        // (we just stored it) and drop the guard immediately so the
        // rest of this function stays lock-free. We grab two things:
        // the UPnP renderer (Some only for a UPnP session — used to
        // read the speaker's level, a SOAP-only path) and the friendly
        // name (works for either protocol — used for the Windows
        // endpoint label).
        let new_upnp_renderer = match guard.as_ref() {
            Some(ActiveSession::Upnp(s)) => Some(s.renderer.clone()),
            _ => None,
        };
        #[cfg(windows)]
        let new_friendly_name = guard.as_ref().map(|s| s.friendly_name());
        drop(guard);
        // The old session — and with it its privacy grant — is gone:
        // cut off any listener the allow-set no longer covers.
        self.prune_stream_clients();

        *self.last_speaker_id.lock().unwrap() = Some(actual_id.clone());
        *self.session_started.lock().unwrap() = Some((actual_id.clone(), Instant::now()));
        // Persist so the next launch can auto-reconnect. We do NOT
        // auto-dismiss the onboarding here — picking a speaker only
        // proves step 2 is done; step 1 (routing Windows audio to
        // Stream To Speaker) is the actual prerequisite for audio
        // to flow, and a user can complete step 2 without realising
        // they still need step 1. The card stays visible until they
        // click "Hide this guide" themselves.
        // (Not a receiver announcing another speaker's identity: the next
        // launch must not connect to it by itself.)
        if crate::engine::cancel::binding().is_none() && !is_newcomer && !crate::airplay::discovery::is_instance_id(&actual_id) {
            let mut uc = self.user_config.lock().unwrap();
            if uc.last_speaker_id.as_deref() != Some(actual_id.as_str()) {
                uc.last_speaker_id = Some(actual_id.clone());
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
        // Start the window, the Windows slider and the speaker out
        // agreeing, on the speaker's level. Without this the window would
        // wait for the first GENA NOTIFY (which can take a few seconds and
        // is silent if the user never adjusts the volume on the speaker
        // side), and the first Windows key press would step from whatever
        // the Windows slider showed before. A UPnP speaker is asked on a
        // detached thread — the call can block ~100 ms.
        if let Some(level) = new_level {
            self.speaker_level_on_connect(session_number, level);
        }
        if let Some(r) = new_upnp_renderer {
            let app = Arc::clone(self);
            std::thread::spawn(move || {
                // Group sessions read the group volume — that's what the
                // slider will be driving.
                let level = match r.group_volume_control_url() {
                    Some(url) => upnp::get_group_volume(url),
                    None => upnp::get_volume(&r.rendering_control_control_url),
                };
                if let Ok(level) = level {
                    app.speaker_level_on_connect(session_number, level);
                }
            });
        }
        // An AirPlay session starts at a level, never muted: carry over a
        // mute set in Windows. (Mute on a UPnP speaker is its own state,
        // which its events report.)
        if new_airplay && self.vsync.is_muted() {
            self.speaker_control.send(SpeakerCommand::Mute(true));
        }
        Ok(())
    }

    /// Make `session` the bound one, under the session lock held in
    /// `guard`. Volume changes still queued for the old speaker are
    /// dropped rather than sent to this one, and nothing sent to the old
    /// one is taken for an echo from this one. Returns the new session's
    /// number (see [`SpeakerControl`]).
    fn bind_session(&self, guard: &mut std::sync::MutexGuard<'_, Option<ActiveSession>>, session: ActiveSession) -> u64 {
        **guard = Some(session);
        let number = self.speaker_control.session_bound();
        if let Some(job) = crate::engine::cancel::binding() {
            job.number.store(number, Ordering::Release);
        }
        self.vsync.speaker_bound();
        number
    }

    fn start_upnp(&self, id: &str) -> Result<(ActiveSession, DeviceUse), String> {
        let discovery = self
            .discovery
            .as_ref()
            .ok_or_else(|| "UPnP discovery disabled".to_string())?;
        if let Some(addrs) = discovery.identity_conflict(id) {
            let addrs: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
            return Err(format!(
                "Several different devices on the network ({}) claim to be this speaker, so none was \
                 picked automatically. Pick the entry with the right address from the speaker list.",
                addrs.join(", ")
            ));
        }
        let Some(new_r) = discovery.find_by_id(id) else {
            return Err(format!("no UPnP speaker with id {:?}", id));
        };
        // Sonos: the discovery list can be minutes stale, and streaming
        // to a zone that has since been grouped would silently rip it
        // out of its group. Re-check topology from the device itself and
        // stream to the group coordinator (= the whole group) instead.
        // No-op for non-Sonos renderers; fails open on network errors.
        // Discovery found this speaker, so the coordinator its topology
        // names is followed only to where discovery's own requests go.
        let new_r = {
            let d = discovery.clone();
            let lan = crate::discovery_net::LanTargets::current();
            let reach = crate::discovery_net::Reach { lan: &lan, reported: &discovery.memory().reported };
            crate::sonos::resolve_group_coordinator(new_r, &move |cid| d.find_by_id(cid), Some(reach))
        };
        let settings_id = new_r.stable_id();
        let (session, device) = self.start_upnp_session(new_r, &settings_id)?;
        Ok((ActiveSession::Upnp(session), device))
    }

    /// Hand `r` the stream and start it playing: offer the stream
    /// profiles chosen for it in turn (see [`Self::stream_offer`]),
    /// remember the one it took under `settings_id`, and watch for it
    /// fetching the stream.
    fn start_upnp_session(&self, r: Renderer, settings_id: &str) -> Result<(RendererSession, DeviceUse), String> {
        let device = self.wait_for_release(settings_id, Some(&r))?;
        // Grant the speaker's addresses in the stream gate (what privacy
        // mode admits) BEFORE the SOAP handshake — it HEAD/GETs the
        // stream while SetAVTransportURI and Play are answered, while
        // `self.session` still holds the old state.
        // The grant moves into the session it produces; if bring-up
        // fails it drops on the error path and the gate forgets it.
        // (`stream_peers` includes the other Sonos group members' addresses
        // resolved by the caller, so the whole group is admitted, whichever
        // unit ends up fetching.)
        let grant = self.stream_gate.grant(r.stream_peers());
        // What was removed from or added to its delay applies from its
        // first fetch, which can come before Play is answered.
        self.delay_for_upnp(settings_id);
        // The URLs the speaker calls back on carry the address this PC
        // has on the speaker's network, not the default-route one.
        let local_ip = self.local_ip_toward(r.ip).map_err(|e| e.to_string())?;
        let (stream_base, callback_url) = local_urls(local_ip, self.config.bind.port());
        let offer = self.stream_offer(&r, settings_id, stream_base);
        let (name, ip, peers) = (r.display_name(), r.ip, r.stream_peers());
        struct ClearDetail<'a>(&'a Mutex<Option<String>>);
        impl Drop for ClearDetail<'_> {
            fn drop(&mut self) {
                *self.0.lock().unwrap() = None;
            }
        }
        let _clear = ClearDetail(&self.connect_detail);
        let progress = |step: ConnectStep| *self.connect_detail.lock().unwrap() = step.text();
        if let Some(level) = crate::engine::cancel::welcome_level() {
            check_job()?;
            let level = level.min(self.volume_limit(settings_id).unwrap_or(100));
            VolumeControl::upnp(&r).set_volume_pct(level.into()).map_err(|e| format!("{e:#}"))?;
        }
        let Started { session, remember } = start_session(r, &offer, &callback_url, Some(grant), &progress)
            .map_err(|e| self.upnp_failure_message(&name, ip, &e))?;
        let mut settings = self.speaker_settings(settings_id);
        if settings.upnp_stream_profile != remember {
            settings.upnp_stream_profile = remember;
            self.set_speaker_settings(settings_id, settings);
        }
        self.watch_first_fetch(name, peers, session.profile, session.stream_given_at, session.played_at);
        Ok((session, device))
    }

    /// What `r` is offered (see [`stream_profile::plan`]): a Sonos the
    /// original stream only; any other renderer the profile it played last
    /// time (`settings_id`'s), if any, and the original stream, and when
    /// those are refused, the types it lists.
    fn stream_offer(&self, r: &Renderer, settings_id: &str, base_url: String) -> StreamOffer {
        let remembered = if r.is_sonos() { None } else { self.speaker_settings(settings_id).upnp_stream_profile };
        StreamOffer { base_url, remembered, initial_buffer_ms: self.config.initial_buffer_ms, budget: CONNECT_BUDGET }
    }

    /// Log when `name` comes for the stream it was given at `given_at`,
    /// or warn when it hasn't by [`FETCH_EXPECTED_WITHIN`] after it
    /// accepted Play at `played_at`: a renderer that took the URI and Play
    /// may still never ask for it.
    fn watch_first_fetch(&self, name: String, peers: Vec<IpAddr>, profile: StreamProfile, given_at: Instant, played_at: Instant) {
        let hub = self.hub.clone();
        let spawned = std::thread::Builder::new().name("stream-to-speaker-fetch-watch".into()).spawn(move || {
            let wait = FETCH_EXPECTED_WITHIN.saturating_sub(played_at.elapsed());
            match hub.wait_for_fetch(&peers, profile, given_at, wait) {
                Some(f) => info!(
                    "UPnP: {} started fetching the stream {:.1} s after it was given it",
                    name,
                    f.at.saturating_duration_since(given_at).as_secs_f32()
                ),
                None => warn!(
                    "UPnP: {} accepted Play but hadn't fetched the stream {} s later; it may stay silent",
                    name,
                    FETCH_EXPECTED_WITHIN.as_secs()
                ),
            }
        });
        if let Err(e) = spawned {
            debug!("not watching for {}'s stream fetch: {}", profile, e);
        }
    }

    /// A speaker added by its device-description URL, as its description
    /// says now (its service URLs used as given: the person chose this
    /// device).
    fn describe_manual(&self, m: &ManualSpeaker) -> Result<Renderer, String> {
        // Fetch from an IPv4 address: the stream and the SOAP control
        // paths are IPv4-only, and a hostname may resolve IPv6-first.
        let mut url = url::Url::parse(&m.host).map_err(|e| format!("{}: {}", m.host, e))?;
        let host = url.host_str().unwrap_or_default().to_string();
        let ip = manual_speakers::resolve_ipv4(&host, url.port().unwrap_or(80))?;
        url.set_ip_host(IpAddr::V4(ip))
            .map_err(|_| format!("{}: can't use address {}", m.host, ip))?;
        crate::ssdp::fetch_and_parse_device(url.as_str(), Duration::from_secs(3), crate::ssdp::ServiceUrls::AsGiven)
            .map_err(|e| format!("couldn't read {}: {:#}", m.host, e))
    }

    /// [`Self::describe_manual`], when it can be read.
    fn manual_renderer(&self, m: &ManualSpeaker) -> Option<Renderer> {
        self.describe_manual(m).map_err(|e| debug!("{}", e)).ok()
    }

    /// UPnP session to a speaker added by its device-description URL: the
    /// description is fetched now, as discovery would, except that its
    /// service URLs are used as given (the user chose this device), and
    /// the session keeps the manual id.
    fn start_upnp_manual(&self, m: &ManualSpeaker) -> Result<(ActiveSession, DeviceUse), String> {
        let r = self.describe_manual(m)?;
        let own_udn = r.udn.clone();
        // A Sonos that's grouped streams via its coordinator, as for a
        // discovered one; the coordinator is looked up in discovery. Its
        // group may sit wherever it does (another subnet, a VLAN, this
        // PC), so the coordinator is followed wherever its topology says.
        let r = match self.discovery.clone() {
            Some(d) => crate::sonos::resolve_group_coordinator(r, &move |cid| d.find_by_id(cid), None),
            None => r,
        };
        let redirected = r.udn != own_udn;
        let settings_id = if redirected { r.stable_id() } else { m.id() };
        let (mut session, device) = self.start_upnp_session(r, &settings_id)?;
        if !redirected {
            session.id_override = Some(m.id());
        }
        Ok((ActiveSession::Upnp(session), device))
    }

    /// What the person is told when starting the UPnP speaker `name` at
    /// `ip` failed with `e`: the app's own words for what the speaker said
    /// (see [`upnp::ActionError::for_person`]), or that it was too slow.
    fn upnp_failure_message(&self, name: &str, ip: IpAddr, e: &StartError) -> String {
        if let Some(budget) = e.out_of_time {
            return format!(
                "{name} was too slow to answer, so Stream To Speaker stopped trying after {}. Try again in a \
                 moment; if it keeps happening, restart {name}.",
                upnp::seconds(budget)
            );
        }
        match e.error.downcast_ref::<upnp::ActionError>() {
            Some(failure) => failure.for_person(name, self.has_airplay_entry_at(ip), e.formats_tried),
            None => format!("{:#}", e.error),
        }
    }

    /// Whether the speaker list has a usable AirPlay entry at `ip` (the
    /// same device, reachable over AirPlay too).
    fn has_airplay_entry_at(&self, ip: IpAddr) -> bool {
        self.airplay_discovery
            .as_ref()
            .is_some_and(|d| d.renderers().iter().any(|r| r.ip == ip && r.transport().is_some()))
    }

    /// AirPlay session to a discovered receiver (`manual` None) or to one
    /// added by address, which is resolved and probed first.
    fn start_airplay(
        &self,
        id: &str,
        manual: Option<&ManualSpeaker>,
    ) -> Result<(ActiveSession, DeviceUse), SelectFailure> {
        self.start_airplay_from(id, manual, &self.hub)
    }

    /// [`Self::start_airplay`], the session taking its sound from `hub`.
    fn start_airplay_from(
        &self,
        id: &str,
        manual: Option<&ManualSpeaker>,
        hub: &crate::http_server::StreamHub,
    ) -> Result<(ActiveSession, DeviceUse), SelectFailure> {
        let (renderer, discovery) = match manual {
            Some(m) => (self.resolve_manual_airplay(m).map_err(SelectFailure::Msg)?, None),
            None => {
                let discovery = self
                    .airplay_discovery
                    .as_deref()
                    .ok_or_else(|| SelectFailure::Msg("AirPlay discovery disabled".to_string()))?;
                let Some(renderer) = discovery.find_by_id(id) else {
                    return Err(SelectFailure::Msg(format!("no AirPlay speaker with id {:?}", id)));
                };
                (renderer, Some(discovery))
            }
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

        if let Some(pair) = renderer.pair.as_ref().filter(|p| !p.is_complete()) {
            return Err(SelectFailure::Msg(format!(
                "{} is a stereo pair; waiting for the other HomePod to come online ({} of {} present).",
                renderer.friendly_name,
                pair.members.len(),
                pair.expected
            )));
        }

        // The local address to bind UDP sockets to and advertise in SDP:
        // the one on the receiver's network (or `--advertise-ip`). It must
        // be reachable by the receiver, so 0.0.0.0 would be wrong.
        let local_ip = self.local_ip_toward(renderer.ip).map_err(SelectFailure::Msg)?;

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
        let mut pin_target: Option<(String, String, PinReason)> = None;
        for (transport, r) in attempts {
            // A newer choice superseded this connect: no further attempt.
            if crate::engine::cancel::cancelled() {
                return Err(SelectFailure::Msg(SUPERSEDED.to_string()));
            }
            let label = match transport {
                Transport::AirPlay2 => "AirPlay 2",
                Transport::RaopLegacy => "AirPlay/RAOP",
            };
            info!("AirPlay: attempting {} to {}", label, r.friendly_name);
            let name = r.friendly_name.clone();
            let id = r.stable_id();
            let device = self.wait_for_release(&id, None).map_err(SelectFailure::Msg)?;
            match self.start_airplay_one(transport, r, local_ip, hub) {
                Ok(session) => return Ok((session, device)),
                Err(AttemptError::NeedsPin(reason)) => {
                    warn!("AirPlay {} to {} needs one-time PIN pairing ({:?})", label, name, reason);
                    pin_target = Some((id, name.clone(), reason));
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
            Some((id, name, reason)) => Err(SelectFailure::NeedsPin { id, name, reason, details }),
            None => Err(SelectFailure::Msg(details)),
        }
    }

    /// Build the receiver record for a manual AirPlay entry: resolve the
    /// host, then ask the receiver what it is (`GET /info`). Cached for
    /// later lookups by id (PIN pairing).
    fn resolve_manual_airplay(&self, m: &ManualSpeaker) -> Result<AirPlayRenderer, String> {
        let ip = manual_speakers::resolve_ipv4(&m.host, m.port)?;
        let addr = SocketAddr::new(IpAddr::V4(ip), m.port);
        let info = manual_speakers::probe_info(addr, Duration::from_secs(3))?;
        match &info {
            Some(i) => info!(
                "manual AirPlay {}: /info model={:?} features={:?}",
                addr, i.model, i.features
            ),
            None => info!("manual AirPlay {}: no /info answer — treating it as AirPlay 1", addr),
        }
        let r = manual_speakers::airplay_renderer(m, ip, info.as_ref());
        self.manual_resolved
            .lock()
            .unwrap()
            .insert(r.stable_id(), r.clone());
        Ok(r)
    }

    /// Ordered list of AirPlay paths to try for a selected device, best
    /// first. Receivers that advertise PTP / transient pairing (HomePod,
    /// Sonos) are tried over AirPlay 2 first; legacy receivers over RAOP
    /// first. The other path is always appended as a fallback, and an
    /// AirPlay 2 sibling at the same IP is used when the `_raop` and
    /// `_airplay` records didn't merge into one entry.
    /// `discovery` is None for a manually added receiver, which is never
    /// paired up with a discovered one.
    fn airplay_attempts(
        &self,
        renderer: &AirPlayRenderer,
        discovery: Option<&AirPlayDiscoveryState>,
    ) -> Vec<(Transport, AirPlayRenderer)> {
        // A newcomer is played over what it announced itself and nothing
        // else: no sibling record is borrowed for it.
        if renderer.is_newcomer() {
            return renderer.transport().map(|t| vec![(t, renderer.clone())]).unwrap_or_default();
        }
        // A stereo pair plays over AirPlay 2 on every member, or not at all.
        if renderer.pair.is_some() {
            return renderer
                .transport()
                .map(|t| vec![(t, renderer.clone())])
                .unwrap_or_default();
        }
        // An AirPlay 2-capable view of this device: the record itself, or a
        // sibling _airplay._tcp entry at the same IP (covers a _raop vs
        // _airplay `deviceid` mismatch that split it into two entries).
        let ap2 = if renderer.supports_airplay2() {
            Some(renderer.clone())
        } else {
            discovery.and_then(|d| {
                d.renderers()
                    .into_iter()
                    .find(|r| r.ip == renderer.ip && r.supports_airplay2() && !r.is_newcomer())
            })
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
        hub: &crate::http_server::StreamHub,
    ) -> Result<ActiveSession, AttemptError> {
        let start_volume = self.airplay_start_volume(&renderer.stable_id());
        match transport {
            Transport::RaopLegacy => {
                let samples_rx = hub.subscribe();
                let stable_id = renderer.stable_id();
                // Nothing stored for a speaker goes to a newcomer: only a
                // password typed for it, once.
                let stored = !renderer.is_newcomer();
                let (password, entered) = self.airplay_password_for(&renderer);
                let latency_ms = self.delay_for_airplay(&renderer).latency_ms;
                let (mfi_encryption, uncompressed_alac, opener_hint) = {
                    let uc = self.user_config.lock().unwrap();
                    (
                        uc.airplay_mfi_encryption,
                        uc.airplay_uncompressed_alac,
                        uc.speaker_settings.get(&stable_id).and_then(|s| s.airplay_opener).filter(|_| stored),
                    )
                };
                // The receiver's other announced addresses, tried in turn
                // when the TCP connection can't be made.
                let lan = crate::discovery_net::LanTargets::current();
                let routes = crate::airplay::session::routes_for(
                    &renderer,
                    crate::airplay::session::Route { receiver: renderer.ip, local: local_ip },
                    |a| crate::airplay::discovery::may_contact(a, &lan),
                    |a| self.local_ip_toward(a).ok(),
                );
                // A connect that takes long says what it waits for, in
                // the notice the connecting banner clears when it ends.
                let name = renderer.friendly_name.clone();
                let notice = |detail: &str| self.record_error(format!("{}{} — {}", STILL_CONNECTING_PREFIX, name, detail));
                let session = AirPlaySession::start_with_notices(
                    AirPlaySessionConfig {
                        renderer,
                        routes,
                        samples_rx,
                        initial_volume: Some(start_volume),
                        timeouts: crate::airplay::session::RaopTimeouts::STANDARD,
                        mfi_encryption,
                        uncompressed_alac,
                        password,
                        latency_ms,
                        opener_hint,
                    },
                    &notice,
                );
                // Start with the opener that worked last time, until it fails.
                let remembered = crate::airplay::session::next_opener_memory(session.as_ref().ok().map(|s| s.opener()));
                if stored && remembered != opener_hint {
                    let mut settings = self.speaker_settings(&stable_id);
                    settings.airplay_opener = remembered;
                    self.set_speaker_settings(&stable_id, settings);
                }
                if let Some(s) = session.as_ref().err().and_then(|e| e.downcast_ref::<crate::airplay::session::SignInNeeded>()) {
                    if matches!(s.why, crate::airplay::rtsp::SignIn::PasswordNeeded | crate::airplay::rtsp::SignIn::PasswordRejected) {
                        self.airplay_asked_password.lock().unwrap().insert(stable_id.clone());
                        if let Some(job) = crate::engine::cancel::binding() {
                            let _ = job.password_rejected.set(entered);
                        }
                    }
                }
                let session = session.map_err(|e| AttemptError::Other(format!("{:#}", e)))?;
                // Up: RECORD answered, any password accepted.
                if let Some(d) = self.airplay_discovery.as_deref() {
                    d.note_session(&session.renderer);
                }
                Ok(ActiveSession::AirPlay(session))
            }
            Transport::AirPlay2 => {
                let stable_id = renderer.stable_id();
                // Nothing stored for a speaker goes to a newcomer.
                let own = !renderer.is_newcomer();
                let delay = self.delay_for_airplay(&renderer);
                let (prefer_realtime, stored) = {
                    let uc = self.user_config.lock().unwrap();
                    (uc.prefer_realtime_airplay, uc.airplay_pairings.get(&stable_id).filter(|_| own).cloned())
                };
                // Parsed before anything is dialled: a damaged entry is the
                // config's fault, not a refusal from the receiver.
                let stored_pairing = match stored.as_ref().map(StoredPairing::parse).transpose() {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("the stored pairing for {} can't be used: {:#}", stable_id, e);
                        return Err(AttemptError::NeedsPin(PinReason::Damaged(format!("{:#}", e))));
                    }
                };
                let addr = renderer.ip;
                let samples_rx = hub.subscribe();
                match AirPlay2Session::start(AirPlay2SessionConfig {
                    renderer,
                    local_ip,
                    samples_rx,
                    initial_volume: Some(start_volume),
                    prefer_realtime,
                    latency_ms: delay.latency_ms,
                    stored_pairing,
                    homepod_realtime: delay.homepod_realtime,
                    homepod_latency_ms: delay.homepod_latency_ms,
                }) {
                    Ok(session) => {
                        if let Some(d) = self.airplay_discovery.as_deref() {
                            d.note_session(&session.renderer);
                        }
                        Ok(ActiveSession::AirPlay2(session))
                    }
                    Err(e) => Err(ap2_attempt_error(e, addr)),
                }
            }
        }
    }

    fn pair_for_job(self: &Arc<Self>, id: String, name: String) -> Result<(), String> {
        check_job()?;
        let (paired, result) = std::sync::mpsc::channel();
        self.begin_pin_pairing(id, name, Some(paired));
        loop {
            check_job()?;
            match result.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => return check_job(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(CODE_WINDOW_CLOSED.into()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
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
    /// A switch or test chime from the window can ask the person for the PIN.
    ///
    /// [`submit_pin`]: App::submit_pin
    fn begin_pin_pairing(self: &Arc<Self>, id: String, name: String, paired: Option<std::sync::mpsc::Sender<()>>) {
        let picks_seen = self.user_pick_count();
        let cancelled = Arc::new(AtomicBool::new(false));
        // Reserve the single ceremony slot; holding it also blocks
        // re-selects of this device (see select_speaker_opts).
        {
            let mut slot = self.pin_ceremony.lock().unwrap();
            if crate::engine::cancel::cancelled() { return; }
            if let Some(existing) = slot.as_ref() {
                if existing.id == id { return; }
                existing.cancelled.store(true, Ordering::Release);
            }
            *slot = Some(PinCeremonyState {
                id: id.clone(),
                name: name.clone(),
                stage: PinStage::Starting,
                cancelled: cancelled.clone(),
                code_try: None,
            });
        }
        let guard = CeremonyGuard(self.clone(), cancelled.clone());

        let Some(renderer) = self.find_airplay_renderer(&id) else {
            self.record_error(format!("Can't pair {} — it's no longer discovered.", name));
            return;
        };
        let local_ip = match self.local_ip_toward(renderer.ip) {
            Ok(ip) => ip,
            Err(e) => {
                self.record_error(format!("Can't pair {}: {}", name, e));
                return;
            }
        };

        let job = crate::engine::cancel::binding();
        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-ap2-pair".into())
            .spawn(move || {
                let _scope = job.as_ref().map(|j| j.enter());
                let _asking = crate::engine::cancel::asking_speaker();
                let guard = guard;
                let port = renderer.airplay_port.unwrap_or(DEFAULT_AIRPLAY_PORT);

                // Connection 1: ask the receiver to display its PIN. A
                // fresh connection each time: after a wrong code, and when
                // the person asks for a new one, it is asked again.
                let check_ceremony = || {
                    if cancelled.load(Ordering::Acquire) { return Err(anyhow!(SUPERSEDED)); }
                    check_job().map_err(anyhow::Error::msg)
                };
                let show_code = || -> Result<()> {
                    check_ceremony()?;
                    let mut rtsp = Ap2Rtsp::connect(renderer.ip, port, local_ip, Duration::from_secs(5))?;
                    check_ceremony()?;
                    if let Err(e) = rtsp.get_info() {
                        debug!("pairing GET /info failed (continuing): {:#}", e);
                    }
                    check_ceremony()?;
                    rtsp.pin_start()
                };
                let mut wrong_code = None;
                loop {
                    if cancelled.load(Ordering::Acquire) || check_job().is_err() { return; }
                    // The dialog says when the TV didn't show a code, and offers to ask again.
                    let shown = show_code().map_err(|e| {
                        info!("{} didn't show a code: {:#}.{}", name, e, forget_pairing_hint(&name, app.has_airplay_pairing(&id)));
                    }).is_ok();
                    let (answers, rx) = crossbeam_channel::bounded(1);
                    let now = Instant::now();
                    {
                        let mut slot = app.pin_ceremony.lock().unwrap();
                        let Some(state) = slot.as_mut().filter(|s| Arc::ptr_eq(&s.cancelled, &cancelled)) else { return };
                        state.code_try = match wrong_code {
                            Some(true) => Some(CodeTry::Wrong { shown }),
                            None if shown => None,
                            _ => Some(CodeTry::NewCode { shown }),
                        };
                        state.stage = PinStage::AwaitingPin { answers, expires: now + Duration::from_secs(120) };
                    }
                    // The code on the TV expires before the person's request does.
                    let idle_end = now + Duration::from_secs(600);
                    let pin = loop {
                        if cancelled.load(Ordering::Acquire) || check_job().is_err() || app.is_shutting_down() { return; }
                        if job.is_none() && Instant::now() >= idle_end {
                            app.record_error(format!("PIN entry for {} timed out — select it again to retry.", name));
                            return;
                        }
                        match rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(PinAnswer::Code(pin)) => break Some(pin),
                            Ok(PinAnswer::NewCode) => break None,
                            Ok(PinAnswer::Cancel) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        }
                    };
                    let Some(pin) = pin else { wrong_code = Some(false); continue };

                    // Stage is Pairing now (submit_pin flipped it). Claim
                    // the "Connecting…" banner while pair-setup runs so
                    // competing selects are refused (best-effort — if
                    // another connect owns it, proceed without).
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
                    // identity is per-install and persistent: re-pairing
                    // with the same controller id REPLACES the accessory's
                    // stored pairing record instead of leaking one of its
                    // finite pairing slots per ceremony.
                    let (ctrl_id, ctrl_seed_hex) = app.airplay_controller_identity();
                    let setup = (|| -> Result<PairingCredentials> {
                        check_ceremony()?;
                        let mut rtsp =
                            Ap2Rtsp::connect(renderer.ip, port, local_ip, Duration::from_secs(5))?;
                        check_ceremony()?;
                        if let Err(e) = rtsp.get_info() {
                            debug!("pairing GET /info failed (continuing): {:#}", e);
                        }
                        check_ceremony()?;
                        rtsp.pair_setup_pin(pin.trim(), &ctrl_id, &ctrl_seed_hex)
                    })();
                    if claimed_banner {
                        *app.connecting.lock().unwrap() = None;
                    }

                    let ended = cancelled.load(Ordering::Acquire) || check_job().is_err();
                    match setup {
                        // The code was typed: the pairing is kept even when
                        // nobody waits for it any more.
                        Ok(creds) => {
                            if !app.store_airplay_pairing(&id, creds) {
                                app.record_error(format!(
                                    "Paired with {name}, but the pairing can't be saved with the \
                                     settings: the PIN will be asked for again after the app restarts."
                                ));
                            }
                            info!("AirPlay 2: paired {}", name);
                            // Release the ceremony slot BEFORE reconnecting —
                            // the select guard refuses selects for the
                            // pairing device while the slot is held.
                            drop(guard);
                            if ended {
                                return;
                            }
                            if let Some(paired) = paired {
                                let _ = paired.send(());
                                return;
                            }
                            app.play_after_pairing(&id, picks_seen);
                            return;
                        }
                        Err(_) if ended => return,
                        // A wrong code: the device is asked for a new one,
                        // and the window waits for it.
                        Err(e) if format!("{e:#}").contains("wrong PIN") => {
                            info!("the code for {} didn't work: {:#}; asking it for a new one", name, e);
                            wrong_code = Some(true);
                            let mut slot = app.pin_ceremony.lock().unwrap();
                            let Some(s) = slot.as_mut().filter(|s| Arc::ptr_eq(&s.cancelled, &cancelled)) else { return };
                            s.stage = PinStage::Starting;
                        }
                        Err(e) => {
                            app.record_error(format!(
                                "Pairing {} failed: {:#} (check the PIN and try again).{}",
                                name,
                                e,
                                forget_pairing_hint(&name, app.has_airplay_pairing(&id))
                            ));
                            return;
                        }
                    }
                }
            })
            .ok();
    }

    pub(crate) fn play_after_pairing(self: &Arc<Self>, id: &str, picks_seen: u64) {
        if let Some(sink) = self.command_sink.get() {
            if self.user_pick_count() == picks_seen {
                let _ = sink(crate::model::Surface::Panel, crate::model::Command::PlayAfterPairing {
                    speaker: crate::model::SpeakerId::new(id), picks_seen,
                });
            }
            return;
        }
        // The original connect may still hold the connecting slot.
        for _ in 0..300 {
            if self.is_shutting_down() { return; }
            if self.connecting.lock().unwrap().is_none() { break; }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.select_speaker_async_opts(id, false, false);
    }

    /// What the last code typed for the pairing in flight, or the last ask
    /// for a new code, came to; None before either.
    pub fn pin_code_try(&self) -> Option<CodeTry> {
        self.pin_ceremony.lock().unwrap().as_ref().and_then(|s| s.code_try)
    }

    /// Ask the device that is being paired to show a new code (*Show a new
    /// code on Den TV*). False when no pairing waits for a code.
    pub fn ask_for_new_code(&self, id: &str) -> bool {
        self.answer_pin(id, PinAnswer::NewCode)
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

    /// Persist HomeKit pairing credentials for a device id (never under a
    /// newcomer's id). Whether it was saved: a pairing that isn't needs
    /// the PIN again after the app restarts.
    pub fn store_airplay_pairing(&self, id: &str, creds: PairingCredentials) -> bool {
        if crate::airplay::discovery::is_instance_id(id) {
            debug!("not storing a pairing under {id}: it names a receiver announcing another speaker's identity");
            return false;
        }
        {
            let mut uc = self.user_config.lock().unwrap();
            uc.airplay_pairings.insert(id.to_string(), creds);
            uc.save();
        }
        !self.settings_unsaved()
    }

    /// Whether this PC has a stored PIN pairing with device `id`. The
    /// device's ⚙ settings then offer to forget it, whichever speaker is
    /// saved: the pairing belongs to that device, and the saved speaker
    /// may be another one (or none).
    pub fn has_airplay_pairing(&self, id: &str) -> bool {
        self.user_config.lock().unwrap().airplay_pairings.contains_key(id)
    }

    /// Forget the stored HomeKit pairing for a device id — only ever on the
    /// user's request. No answer from the network deletes a stored pairing
    /// (none is authenticated), and a new PIN pairing replaces it, so this
    /// is the way out for a device that stopped asking for one. A live
    /// session keeps its keys; the next connect pairs from scratch.
    pub fn forget_airplay_pairing(&self, id: &str) {
        info!("forgetting the stored AirPlay pairing for {} at the user's request", id);
        let mut uc = self.user_config.lock().unwrap();
        if uc.remove_airplay_pairing(id) {
            uc.save();
        }
    }

    /// The local address to give a speaker at `peer`: `--advertise-ip` when
    /// set, else a specific `--bind` address, else the address Windows
    /// would send from to reach `peer`, so a
    /// speaker on a second NIC is not handed a VPN or other-subnet address.
    pub fn local_ip_toward(&self, peer: IpAddr) -> Result<IpAddr, String> {
        if let Some(ip) = self.config.advertise_ip {
            return Ok(ip);
        }
        // `--bind` to a specific address: the HTTP server only listens
        // there, so that is the address speakers must be given.
        let bind_ip = self.config.bind.ip();
        if !bind_ip.is_unspecified() {
            return Ok(bind_ip);
        }
        route_source_ip(peer).ok_or_else(|| format!("this PC has no network route to {}", peer))
    }

    /// The address that names this PC to other devices now (it follows
    /// network changes; see [`this_pc_address_from`]).
    pub fn this_pc_address(&self) -> IpAddr {
        let adapter = match &*self.discovery_scope.lock().unwrap() {
            Resolved::Active { ip, .. } => Some(*ip),
            _ => None,
        };
        this_pc_address_from(
            self.config.advertise_ip,
            self.config.bind.ip(),
            adapter,
            || {
                let speaker = self.session.lock().unwrap().as_ref().map(|s| s.ip());
                speaker.filter(|ip| !ip.is_unspecified()).and_then(route_source_ip)
            },
            default_route_ip,
            first_local_ipv4,
        )
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
            Some(s) if matches!(s.stage, PinStage::AwaitingPin { .. }) => {
                Some((s.id.clone(), s.name.clone()))
            }
            _ => None,
        }
    }

    pub(crate) fn pairing_code_ask(&self) -> Option<crate::settings::port::CodeAsk> {
        use crate::settings::port::{CodeAsk, CodeStage};
        let slot = self.pin_ceremony.lock().unwrap();
        let state = slot.as_ref()?;
        if state.cancelled.load(Ordering::Acquire) { return None; }
        let (stage, seconds_left) = match &state.stage {
            PinStage::Starting => (CodeStage::Starting, None),
            PinStage::Pairing => (CodeStage::Checking, None),
            PinStage::AwaitingPin { expires, .. } => {
                let left = expires.saturating_duration_since(Instant::now());
                match state.code_try {
                    Some(CodeTry::Wrong { shown: false } | CodeTry::NewCode { shown: false }) => (CodeStage::NoAnswer, None),
                    _ if left.is_zero() => (CodeStage::TimedOut, None),
                    last => {
                        let stage = if matches!(last, Some(CodeTry::Wrong { .. })) { CodeStage::Wrong } else { CodeStage::Showing };
                        (stage, Some(left.as_millis().div_ceil(1000) as u32))
                    }
                }
            }
        };
        Some(CodeAsk { id: state.id.clone(), name: state.name.clone(), stage, seconds_left })
    }

    /// GUI: deliver the entered PIN (`Some`) or cancel the ceremony
    /// (`None`). Wakes the parked ceremony thread. Returns false when no
    /// ceremony was awaiting a PIN (it expired or was never started) —
    /// the caller should tell the user to re-select the speaker, since a
    /// PIN typed into a stale modal would otherwise vanish silently.
    pub fn submit_pin(&self, id: &str, pin: Option<String>) -> bool {
        self.answer_pin(id, pin.map_or(PinAnswer::Cancel, PinAnswer::Code))
    }

    /// Give the pairing waiting for its code `answer`.
    fn answer_pin(&self, id: &str, answer: PinAnswer) -> bool {
        let mut guard = self.pin_ceremony.lock().unwrap();
        let Some(state) = guard.as_mut().filter(|s| s.id == id && !s.cancelled.load(Ordering::Acquire)) else {
            return false;
        };
        let next = match answer {
            PinAnswer::Code(_) => Some(PinStage::Pairing),
            PinAnswer::NewCode => Some(PinStage::Starting),
            PinAnswer::Cancel => {
                info!("PIN pairing for {} cancelled by user", state.name);
                state.cancelled.store(true, Ordering::Release);
                None
            }
        };
        // The dialog offers no code to enter or renew while it asks the TV
        // or checks a code. A code or renewal arriving then changes nothing.
        let PinStage::AwaitingPin { answers, .. } = &state.stage else { return true };
        // The ceremony checks for a cancel by itself too.
        if answers.try_send(answer).is_err() {
            return next.is_none();
        }
        if let Some(stage) = next {
            state.stage = stage;
        }
        true
    }

    /// Current speaker-side volume (0-100) if known: None until a level
    /// came from the Windows slider, the window's slider or a speaker.
    pub fn current_volume(&self) -> Option<u32> {
        self.vsync.current_level()
    }

    /// The window's volume slider set `level`. Sets the window's level and
    /// the Windows slider, and queues the level for the bound speaker's
    /// volume thread (see [`SpeakerControl`]), so the caller doesn't
    /// block. Moving the slider unmutes, as the Windows volume slider
    /// does. The bound session applies the level.
    pub fn set_speaker_volume(&self, level: u32) {
        self.apply_speaker_level(level, true);
    }

    pub(crate) fn apply_speaker_level(&self, level: u32, unmute: bool) {
        if !self.speaker_volume_controllable() {
            return;
        }
        let level = (level.min(100) as u8).min(self.bound_limit().unwrap_or(100)) as u32;
        let unmute = unmute && self.vsync.is_muted();
        if unmute {
            self.vsync.note_muted(false);
        }
        // Update the level at once so the slider doesn't snap back while
        // the network call is in flight, and move the Windows slider with
        // it: the speaker's report of this level is an echo, so it won't,
        // and the next Windows key press would otherwise step from the old
        // Windows level.
        if self.vsync.window_changed(level) || unmute {
            push_windows_level(&self.windows_volume, level, self.vsync.is_muted());
        }
        // Level first: on AirPlay an unmute restores the session's level,
        // which is then already the new one.
        self.speaker_control.send(SpeakerCommand::Volume(level));
        if unmute {
            self.speaker_control.send(SpeakerCommand::Mute(false));
        }
    }

    /// The bound session can change the speaker’s level.
    pub fn speaker_volume_controllable(&self) -> bool {
        self.session.lock().unwrap().as_ref().is_some_and(ActiveSession::has_volume_control)
    }

    /// The level an AirPlay session starts the speaker at: the current
    /// one, so a reconnect or a switch keeps what the window and Windows
    /// show, else a default.
    /// The level an AirPlay speaker starts at: the one last used with it,
    /// 25 the first time. Never louder: its own level can't be read.
    fn airplay_start_volume(&self, id: &str) -> u32 {
        if let Some(level) = crate::engine::cancel::welcome_level() {
            return u32::from(level.min(self.volume_limit(id).unwrap_or(100)));
        }
        let s = self.speaker_settings(id);
        start_level(s.level, s.level_at, unix_seconds(), self.volume_limit(id)) as u32
    }

    /// A fixed-volume player starts at full scale until the app has a level for it.
    pub(crate) fn sendspin_start_level(&self, id: &str) -> u8 {
        let cfg = self.user_config.lock().unwrap();
        let saved = cfg.speaker_settings.get(id);
        let limit = saved.and_then(|s| s.volume_limit);
        let kept = cfg.sound_place.as_ref().and_then(|p| p.levels.get(&crate::model::SpeakerId::new(id)));
        if let Some(kept) = kept {
            start_level(Some(kept.level), Some(kept.at.0.max(0) as u64 / 1000), unix_seconds(), limit)
        } else if let Some(s) = saved.filter(|s| s.level.is_some()) {
            start_level(s.level, s.level_at, unix_seconds(), limit)
        } else {
            limit.unwrap_or(100).min(100)
        }
    }

    /// The speaker bound as session `session_number` is at `level` as
    /// its session starts: show it in the window and on the Windows
    /// slider, unless another speaker has been bound since or a level was
    /// set for this one meanwhile. Above its *Maximum volume from this
    /// PC*, it is set to that maximum: the maximum caps every start.
    fn speaker_level_on_connect(&self, session_number: u64, level: u32) {
        if self.speaker_control.session_number() != session_number {
            return;
        }
        if let Some(limit) = self.bound_limit().filter(|l| level > *l as u32) {
            info!("the speaker started at {} above its maximum: {}", level, limit);
            self.apply_speaker_level(limit as u32, false);
            return;
        }
        if let Some(level) = self.vsync.speaker_connected(level) {
            info!("speaker -> windows: volume {} (as the session started)", level);
            push_windows_level(&self.windows_volume, level, self.vsync.is_muted());
        }
    }

    /// Register how to set the Windows slider (the driver's volume push).
    /// The first registration wins.
    pub fn set_windows_volume_push(&self, push: impl Fn(i32, bool) -> Result<()> + Send + Sync + 'static) {
        let _ = self.windows_volume.driver.set(Box::new(push));
    }

    /// The speaker reported its level or mute (a Sonos event): Windows'
    /// slider follows, by whichever way the app moves it, unless this is
    /// the echo of the app's own change.
    pub fn speaker_reported_level(&self, volume: Option<u32>, mute: Option<bool>) {
        if let Some((mb, muted)) = self.vsync.speaker_event(volume, mute) {
            push_windows_level(&self.windows_volume, crate::volume_sync::millibels_to_sonos(mb), muted);
        }
    }

    /// Move Windows' slider for the app's output through `set` (level
    /// 0–100, muted) from now on, instead of the driver's push. The first
    /// registration wins.
    pub fn set_windows_level_through(&self, set: impl Fn(u32, bool) + Send + Sync + 'static) {
        self.try_windows_level_through(move |level, muted| { set(level, muted); true });
    }

    /// Fall back to the driver when Windows has no active endpoint for this change.
    pub(crate) fn try_windows_level_through(&self, set: impl Fn(u32, bool) -> bool + Send + Sync + 'static) {
        let _ = self.windows_volume.endpoint.set(Box::new(set));
    }

    /// Mute or unmute the bound speaker from the app, as
    /// [`Self::set_speaker_volume`] sets its level.
    pub fn set_speaker_muted(&self, muted: bool) {
        if !self.session.lock().unwrap().as_ref().is_some_and(ActiveSession::has_volume_control) {
            return;
        }
        self.vsync.note_muted(muted);
        if let Some(level) = self.vsync.current_level() {
            push_windows_level(&self.windows_volume, level, muted);
        }
        self.speaker_control.send(SpeakerCommand::Mute(muted));
    }

    /// Queue a volume or mute change from Windows for the bound speaker.
    /// Returns at once; the send happens on the volume thread.
    pub fn send_to_speaker(&self, cmd: SpeakerCommand) {
        // Above the speaker's limit: the limit goes to the speaker, and
        // Windows' slider springs back to it.
        if let (SpeakerCommand::Volume(level), Some(limit)) = (&cmd, self.bound_limit()) {
            if *level > limit as u32 {
                push_windows_level(&self.windows_volume, limit as u32, self.vsync.is_muted());
                self.speaker_control.send(SpeakerCommand::Volume(limit as u32));
                return;
            }
        }
        self.speaker_control.send(cmd);
    }

    // -------------------------------------------------------------------
    // Discovery network (adapter selection)
    // -------------------------------------------------------------------

    /// Start SSDP (every `ssdp_interval`) and mDNS discovery on the saved
    /// adapter choice, plus the monitor that pauses / resumes them as that
    /// adapter goes down and comes back.
    pub fn start_discovery(self: &Arc<Self>, ssdp_interval: Duration) {
        let adapters = discovery_net::list_adapters();
        let resolved = self.resolve_scope(&adapters);
        info!("discovery: {:?}", resolved);
        *self.adapters_cache.lock().unwrap() = adapters;
        *self.discovery_scope.lock().unwrap() = resolved.clone();

        if let Some(d) = self.discovery.clone() {
            let weak = Arc::downgrade(self);
            let scope: crate::ssdp::ScopeFn = Arc::new(move || {
                weak.upgrade().map(|a| a.ssdp_scope()).unwrap_or(SearchScope::Paused)
            });
            crate::ssdp::spawn_discovery(d, ssdp_interval, scope);
        }
        if let Some(state) = self.airplay_discovery.clone() {
            let found = state.clone();
            let ctl = MdnsController::new(
                "AirPlay",
                move |d| crate::airplay::discovery::spawn_airplay_discovery_on(found.clone(), d),
                move || state.clear(),
            );
            if let Err(e) = ctl.apply(&resolved) {
                self.note_discovery_failure(&e);
            }
            *self.mdns.lock().unwrap() = Some(ctl);
        }

        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-netwatch".into())
            .spawn(move || {
                while app.sleep_unless_shutdown(Duration::from_secs(2)) {
                    app.refresh_discovery_scope();
                }
            })
            .ok();
    }

    /// The AirPlay browse couldn't start. Said once per failure; the
    /// network check retries quietly.
    fn note_discovery_failure(&self, e: &anyhow::Error) {
        let mut error = self.discovery_error.lock().unwrap();
        if error.is_some() {
            debug!("AirPlay discovery: still failing to start: {:#}", e);
            return;
        }
        let msg = format!(
            "AirPlay speaker discovery couldn't start ({:#}). It will keep trying. AirPlay \
             speakers can still be added by address.",
            e
        );
        self.record_error(msg.clone());
        *error = Some(msg);
    }

    /// Why AirPlay speaker discovery isn't running, if it couldn't start.
    pub(crate) fn airplay_discovery_error(&self) -> Option<String> {
        self.discovery_error.lock().unwrap().clone()
    }

    /// Network check: try again to start an AirPlay browse that failed to
    /// start (nothing else restarts it while the adapter scope holds).
    fn retry_failed_airplay_discovery(&self, scope: &Resolved) {
        if self.discovery_error.lock().unwrap().is_none() {
            return;
        }
        if let Some(ctl) = self.mdns.lock().unwrap().as_ref() {
            match ctl.apply(scope) {
                Ok(()) => {
                    *self.discovery_error.lock().unwrap() = None;
                    if ctl.is_running() {
                        info!("AirPlay discovery: started");
                    }
                }
                Err(e) => self.note_discovery_failure(&e),
            }
        }
    }

    /// The saved adapter choice mapped onto `adapters`. A command-line pin
    /// (`--advertise-ip` / `--bind`) overrides it: SSDP then searches from
    /// that address and mDNS from every adapter, as before the choice
    /// existed.
    fn resolve_scope(&self, adapters: &[NetAdapter]) -> Resolved {
        if self.config.ssdp_iface.is_some() {
            return Resolved::All;
        }
        let selected = self.user_config.lock().unwrap().discovery_adapter.clone();
        discovery_net::resolve(selected.as_ref(), adapters)
    }

    /// Discovery's adapter scope as last applied.
    pub(crate) fn current_discovery_scope(&self) -> Resolved {
        self.discovery_scope.lock().unwrap().clone()
    }

    /// Where SSDP searches right now.
    pub fn ssdp_scope(&self) -> SearchScope {
        if let Some(ip) = self.config.ssdp_iface {
            return SearchScope::Iface(ip);
        }
        match &*self.discovery_scope.lock().unwrap() {
            Resolved::All => SearchScope::All,
            Resolved::Active { ip, .. } => SearchScope::Iface(*ip),
            Resolved::Paused { .. } => SearchScope::Paused,
        }
    }

    /// Re-check the adapters and apply any change of scope: restart mDNS
    /// (AirPlay and Sendspin) on the new interface set (or stop it while
    /// paused), forget what was found on the old network, and search again.
    /// Otherwise retry an AirPlay or Sendspin browse that failed to start.
    fn refresh_discovery_scope(self: &Arc<Self>) {
        let _serial = self.scope_apply.lock().unwrap();
        let adapters = discovery_net::list_adapters();
        let new = self.resolve_scope(&adapters);
        *self.adapters_cache.lock().unwrap() = adapters;
        let old = std::mem::replace(&mut *self.discovery_scope.lock().unwrap(), new.clone());
        if old == new {
            self.retry_failed_airplay_discovery(&new);
            self.retry_failed_sendspin_discovery();
            return;
        }
        match &new {
            Resolved::All => info!("discovery: searching on every adapter"),
            Resolved::Active { name, ip } => info!("discovery: searching on {} ({})", name, ip),
            Resolved::Paused { name } => {
                warn!("discovery: paused — adapter {} is unavailable", name)
            }
        }
        if let Some(ctl) = self.mdns.lock().unwrap().as_ref() {
            match ctl.apply(&new) {
                Ok(()) => *self.discovery_error.lock().unwrap() = None,
                Err(e) => self.note_discovery_failure(&e),
            }
        }
        self.sync_sendspin_discovery();
        if let Some(d) = self.discovery.as_ref() {
            d.clear();
        }
        if !matches!(new, Resolved::Paused { .. }) {
            self.trigger_rescan();
        }
    }

    /// Settings: the adapters to offer (as of the last check, at most a
    /// couple of seconds old).
    pub fn network_adapters(&self) -> Vec<NetAdapter> {
        self.adapters_cache.lock().unwrap().clone()
    }

    /// Sets the adapters the pages list.
    #[cfg(test)]
    pub(crate) fn set_network_adapters(&self, adapters: Vec<NetAdapter>) {
        *self.adapters_cache.lock().unwrap() = adapters;
    }

    pub fn discovery_net_status(&self) -> DiscoveryNetStatus {
        DiscoveryNetStatus {
            scope: self.discovery_scope.lock().unwrap().clone(),
            selected: self.user_config.lock().unwrap().discovery_adapter.clone(),
            pinned: self.config.ssdp_iface,
            error: {
                let airplay = self.discovery_error.lock().unwrap().clone();
                match (airplay, self.sendspin_discovery_error()) {
                    (Some(a), Some(s)) => Some(format!("{}\n{}", a, s)),
                    (a, s) => a.or(s),
                }
            },
        }
    }

    /// Persist a new adapter choice (`None` = every adapter) and apply it
    /// off the calling thread (restarting mDNS takes a moment).
    pub fn set_discovery_adapter(self: &Arc<Self>, choice: Option<SavedAdapter>) {
        {
            let mut uc = self.user_config.lock().unwrap();
            if uc.discovery_adapter == choice {
                return;
            }
            uc.discovery_adapter = choice;
            uc.save();
        }
        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-netapply".into())
            .spawn(move || app.refresh_discovery_scope())
            .ok();
    }

    /// Fire a one-shot SSDP M-SEARCH on the same interface the periodic
    /// loop uses. Runs on a detached thread because `discover_once`
    /// blocks for ~3 s waiting for responses; the GUI button doesn't
    /// want to freeze. Results land in `DiscoveryState::replace` and
    /// are picked up by the next `speaker_view()` call.
    pub fn trigger_rescan(self: &Arc<Self>) {
        let Some(discovery) = self.discovery.clone() else {
            warn!("trigger_rescan: discovery disabled");
            return;
        };
        // Skip if a scan is already running — the user can hammer the
        // button but we don't need two scans racing.
        if self.rescan_in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let scope = self.ssdp_scope();
        let Some(iface) = scope.iface() else {
            self.rescan_in_flight.store(false, Ordering::Release);
            if let Resolved::Paused { name } = &*self.discovery_scope.lock().unwrap() {
                self.record_error(format!(
                    "Speaker search is paused: the network adapter \"{}\" is unavailable. \
                     It resumes when the adapter is back; \"Look for speakers on\" under \
                     \"Speaker missing?\" chooses another.",
                    name
                ));
            }
            return;
        };
        let app = self.clone();
        let spawned = std::thread::Builder::new()
            .name("stream-to-speaker-rescan".to_string())
            .spawn(move || {
                match crate::ssdp::discover_once(Duration::from_secs(3), iface, discovery.memory()) {
                    // Searched the old network; the scope moved meanwhile.
                    Ok(_) if app.ssdp_scope() != scope => {}
                    Ok(found) => {
                        let n = found.len();
                        info!("manual rescan: {} renderer(s) found", n);
                        discovery.replace(found);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        app.last_rescan_count.store(n, Ordering::Release);
                        app.last_rescan_finished_unix.store(now, Ordering::Release);
                    }
                    Err(e) => {
                        // A failed scan is not "no speakers found" —
                        // surface it as an error banner instead of the
                        // scan-complete label.
                        app.record_error(format!("Couldn't scan for speakers: {}", e));
                    }
                }
                app.rescan_in_flight.store(false, Ordering::Release);
            });
        if let Err(e) = spawned {
            // If the thread never started, nothing will clear the
            // in-flight flag — reset it here or every later rescan
            // would no-op at the swap guard above.
            self.rescan_in_flight.store(false, Ordering::Release);
            self.record_error(format!("Couldn't start speaker scan: {}", e));
        }
    }

    /// Resync — drop the speaker's accumulated prebuffer.
    ///
    /// UPnP: Stop + Play, Sonos picks a fresh prebuffer level.
    /// AirPlay: tear down and rebuild the session, which is the only
    /// reliable way to flush RAOP's own jitter buffer (FLUSH is more
    /// surgical but receivers differ on what it actually clears).
    pub fn resync(self: &Arc<Self>) -> Result<(), String> {
        if let Some(id) = self.session.lock().unwrap().as_ref().map(|s| s.stable_id()) {
            if self.send_command(
                crate::model::Surface::Panel,
                crate::model::Command::Reconnect(crate::model::SpeakerId::new(&id)),
            ) {
                return Ok(());
            }
        }
        self.resync_now()
    }

    /// Reconnect the bound speaker now, on this thread (*Reconnect*).
    pub(crate) fn resync_now(self: &Arc<Self>) -> Result<(), String> {
        // Look at the active session under the lock briefly to figure
        // out the dispatch, but don't hold the lock during the slow
        // network calls.
        enum Kind {
            Upnp(Renderer, DeviceUse),
            AirPlay(String),
        }
        let kind = {
            let guard = self.session.lock().unwrap();
            match guard.as_ref() {
                Some(session @ ActiveSession::Upnp(s)) => Kind::Upnp(s.renderer.clone(), self.stopping_device(session, None)),
                Some(ActiveSession::AirPlay(s)) => {
                    Kind::AirPlay(s.renderer.stable_id())
                }
                Some(ActiveSession::AirPlay2(s)) => {
                    // Same recovery as AirPlay 1: tear down + reconnect.
                    Kind::AirPlay(s.renderer.stable_id())
                }
                Some(ActiveSession::Sendspin(s)) => {
                    // A fresh session restarts the stream timeline.
                    Kind::AirPlay(s.renderer.stable_id())
                }
                // Music Assistant buffers on its side; nothing to resync here.
                Some(ActiveSession::MusicAssistant(_)) => return Ok(()),
                None => return Err("no active speaker".to_string()),
            }
        };
        match kind {
            Kind::Upnp(r, _device) => {
                info!("resync: UPnP Stop + Play on {}", r.friendly_name);
                if let Err(e) = upnp::stop(&r.av_transport_control_url) {
                    debug!("resync stop ignored: {}", e);
                }
                std::thread::sleep(Duration::from_millis(100));
                let played = upnp::play(&r.av_transport_control_url).map_err(|e| format!("play failed: {:#}", e));
                // The speaker starts over at its own delay: what was removed
                // or added applies again.
                self.delay.catch_up().restarted();
                played
            }
            Kind::AirPlay(id) => {
                info!("resync: AirPlay tear-down + reconnect for {}", id);
                self.select_speaker_now(&id)
            }
        }
    }

    // -------------------------------------------------------------------
    // Enable / disable
    // -------------------------------------------------------------------

    /// User toggle. Disabling tears down the session (freeing the speaker
    /// for other use) and closes `/stream.raw` to every device but this
    /// PC, whatever the privacy setting; enabling rebinds to the last-used
    /// speaker if one is remembered.
    pub fn set_streaming_enabled(self: &Arc<Self>, enabled: bool) -> Result<(), String> {
        if self.command_sink.get().is_some() {
            // Disable is choosing This PC; Enable plays on the speaker used
            // last.
            let last = self.last_speaker_id.lock().unwrap().clone().or_else(|| self.saved_speaker_id());
            let cmd = match (enabled, last) {
                (true, Some(id)) => {
                    // As before: Enable never plays to a receiver announcing
                    // another speaker's identity.
                    if self.is_airplay_newcomer(&id) {
                        return Err(format!(
                            "To play to “{}”, pick it in the Stream To Speaker window.",
                            self.speaker_name(&id).unwrap_or_else(|| id.to_string())
                        ));
                    }
                    crate::model::Command::Switch { place: place_of(&id), way: None }
                }
                (true, None) => return Ok(()),
                (false, _) => crate::model::Command::Switch { place: crate::model::Place::ThisPc, way: None },
            };
            self.send_command(crate::model::Surface::Tray, cmd);
            return Ok(());
        }
        let was = self.streaming_enabled.swap(enabled, Ordering::AcqRel);
        if was == enabled {
            return Ok(());
        }
        if !enabled {
            let s = self.session.lock().unwrap().take().map(|s| self.releasing(s, None));
            if let Some(s) = s {
                s.stop();
            }
            self.prune_stream_clients();
            info!("streaming disabled");
        } else if let Some(name) = self.connecting_to() {
            // A connect is still in flight; with streaming enabled again
            // it binds when it's up, so starting another would only fight it.
            info!("streaming enabled; the connect to {} in progress will bind", name);
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
        self.web_access.is_enabled()
    }

    /// Turn the web page and the JSON API on or off. On needs the secret
    /// their link carries: the stored one, or one made now and saved.
    pub fn set_web_ui_enabled(&self, enabled: bool) -> Result<(), String> {
        if enabled {
            self.load_or_make_web_secret()?;
        }
        if self.web_access.set_enabled(enabled) {
            info!("web UI: {}", if enabled { "enabled" } else { "disabled" });
        }
        Ok(())
    }

    /// The web UI's link, `http://<this PC>:<port>/r/<key>/`, while it is
    /// on. It is the credential: shown in the window and printed by a
    /// headless `--web` run, never logged.
    pub fn web_link(&self) -> Option<String> {
        if !self.web_access.is_enabled() {
            return None;
        }
        let key = self.web_access.secret_hex()?;
        Some(web_access::link(&self.this_pc_address().to_string(), self.config.bind.port(), &key))
    }

    /// A separate bearer key for scripts. Rotating it never invalidates a phone link.
    pub(crate) fn set_scripts_access(&self, enabled: bool, renew: bool) -> Result<(), String> {
        let mut cfg = self.user_config.lock().unwrap();
        let key = if enabled {
            Some(match cfg.scripts_key.as_deref().and_then(WebSecret::parse).filter(|_| !renew) {
                Some(key) => key,
                None => WebSecret::generate()?,
            })
        } else { None };
        let was = cfg.scripts_enabled;
        let old = cfg.scripts_key.clone();
        cfg.scripts_enabled = enabled;
        if let Some(key) = &key { cfg.scripts_key = Some(key.to_hex()); }
        if let Err(e) = cfg.try_save() {
            cfg.scripts_enabled = was;
            cfg.scripts_key = old;
            return Err(e.to_string());
        }
        self.web_access.set_scripts_key(key);
        Ok(())
    }

    /// Replace the web UI's secret: the current link and API key stop
    /// working with their next request. The new secret takes effect only
    /// once config.json holds it: one only this run knew would be undone
    /// by the next start, which brings back the link the user meant to
    /// retire. If it can't be saved, nothing changes and the error says
    /// so.
    pub fn new_web_link(&self) -> Result<(), String> {
        let secret = WebSecret::generate()?;
        let mut uc = self.user_config.lock().unwrap();
        let current = uc.web_secret.replace(secret.to_hex());
        if let Err(e) = uc.try_save() {
            // Not left for the next save of another setting to store
            // either: the link the window still shows would stop working
            // at the next start.
            uc.web_secret = current;
            return Err(format!("the settings couldn't be saved ({e}). The current link still works."));
        }
        self.web_access.set_secret(secret);
        info!("web UI: made a new link; the previous one no longer works");
        Ok(())
    }

    /// Put the web UI's secret in force: the one the settings hold; else
    /// the one config.json holds now, which `--web-link` may have stored
    /// while this run held none; else one made now and saved (first use,
    /// or a value this version can't use). `--web-link` prints the one
    /// config.json holds (see `web_access::stored_or_new_secret`).
    fn load_or_make_web_secret(&self) -> Result<(), String> {
        let secret = web_access::stored_or_new_secret(
            &mut self.user_config.lock().unwrap(),
            UserConfig::stored_web_secret,
            UserConfig::save,
        )?;
        self.web_access.set_secret(secret);
        Ok(())
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

    /// Audio loop: a packet came from Windows (not generated here);
    /// `audible` is whether it carried sound.
    pub fn note_source_packet(&self, audible: bool) {
        let ms = self.started_at.elapsed().as_millis() as u64 + 1;
        self.source_packet_at_ms.store(ms, Ordering::Relaxed);
        if audible {
            self.source_sound_at_ms.store(ms, Ordering::Relaxed);
        }
    }

    /// Windows is giving the audio loop a stream, with sound or silent:
    /// what the window's status and the tray call streaming. The driver's
    /// stream flag alone is set from launch until Windows first stops a
    /// stream, whether anything came or not.
    pub fn receiving_windows_audio(&self) -> bool {
        matches!(self.windows_audio(), WindowsAudio::Playing | WindowsAudio::Silent)
    }

    /// The audio thread lost its source (`why`, for the log and the
    /// diagnostics) and is trying to open it again; speakers get silence
    /// meanwhile, and the window says so.
    pub fn note_sound_input_lost(&self, why: &str) {
        warn!("lost this PC's sound ({}); trying to open the sound device again", why);
        self.record_fault(&format!("Lost this PC's sound: {}", why));
        *self.sound_input_lost.lock().unwrap() = Some(why.to_string());
    }

    /// The audio thread reads this PC's sound again.
    pub fn note_sound_input_back(&self) {
        if self.sound_input_lost.lock().unwrap().take().is_some() {
            info!("this PC's sound is back");
        }
    }

    /// The audio thread can't read this PC's sound and is trying to open
    /// the sound device again.
    pub fn sound_input_lost(&self) -> bool {
        self.sound_input_lost.lock().unwrap().is_some()
    }

    /// The driver's events (Windows volume, mute, its stream starting and
    /// stopping) can't be read any more (`why`, for the log); the consumer
    /// is trying to open them again.
    pub fn note_driver_events_lost(&self, why: &str) {
        warn!("lost the sound device's events ({}); trying to open them again", why);
        // Whether Windows' stream runs is unknown now: let the packets
        // themselves tell the status.
        self.stream_active.store(true, Ordering::Release);
        self.record_fault(&format!("Lost the sound device's events: {}", why));
        self.record_error(
            "Lost touch with the sound device: Windows volume changes won't reach the speaker until it \
             reconnects.",
        );
    }

    /// The driver's events can be read again.
    pub fn note_driver_events_back(&self) {
        info!("the sound device's events are back");
    }

    /// What Windows is giving the audio loop right now.
    pub fn windows_audio(&self) -> WindowsAudio {
        let now = self.started_at.elapsed().as_millis() as u64 + 1;
        let since = |at: &AtomicU64| {
            let at = at.load(Ordering::Relaxed);
            (at != 0).then(|| Duration::from_millis(now.saturating_sub(at)))
        };
        WindowsAudio::classify(
            self.stream_active.load(Ordering::Acquire),
            since(&self.source_packet_at_ms),
            since(&self.source_sound_at_ms),
        )
    }

    // -------------------------------------------------------------------
    // Diagnostics
    // -------------------------------------------------------------------

    /// Counters and state right now. Brief locks only.
    pub fn diagnostics_snapshot(&self) -> Snapshot {
        let (speaker, speaker_id, speaker_addr, transport, resend) = {
            let guard = self.session.lock().unwrap();
            match guard.as_ref() {
                Some(s) => (
                    Some(s.friendly_name()),
                    Some(s.stable_id()),
                    Some(s.ip().to_string()),
                    Some(s.transport_label()),
                    s.resend_stats(),
                ),
                None => (None, None, None, None, None),
            }
        };
        let session_secs = self
            .session_started
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(id, _)| speaker_id.as_deref() == Some(id.as_str()))
            .map(|(_, t)| t.elapsed().as_secs());
        let airplay_buffer_ms = speaker_id
            .as_deref()
            .filter(|id| id.starts_with("airplay:"))
            .map(|id| self.airplay_latency_ms_for(id));
        let discovery = match &*self.discovery_scope.lock().unwrap() {
            _ if self.config.no_discovery && self.airplay_discovery.is_none() => {
                "off".to_string()
            }
            Resolved::All => match self.config.ssdp_iface {
                Some(ip) => format!("SSDP from {}, mDNS on every adapter", ip),
                None => "every adapter".to_string(),
            },
            Resolved::Active { name, ip } => format!("{} ({}) only", name, ip),
            Resolved::Paused { name } => format!("paused ({} unavailable)", name),
        };
        Snapshot {
            taken_unix: now_unix(),
            app_uptime_secs: self.uptime_secs(),
            speaker,
            speaker_id,
            speaker_addr,
            transport,
            session_secs,
            reconnects: self.reconnects.load(Ordering::Relaxed),
            streaming_enabled: self.is_streaming_enabled(),
            windows_audio: self.windows_audio(),
            connecting_to: self.connecting_to(),
            packets_published: self.packets_published(),
            idle_silence_packets: self.idle_silence_packets.load(Ordering::Relaxed),
            dropped_packets: self.hub.dropped_frames(),
            hub_consumers: self.hub.subscriber_count(),
            resend,
            pending_latency_ms: self.pending_latency_ms(),
            airplay_buffer_ms,
            stream_format: "L16 PCM, 44.1 kHz, 16-bit, stereo",
            discovery,
            speakers_listed: self.speaker_view().speakers.len(),
        }
    }

    /// Remember a fault together with the counters just before it.
    pub(crate) fn record_fault(&self, what: &str) {
        let before = self.diagnostics_snapshot();
        *self.last_fault.lock().unwrap() = Some(Fault {
            when_unix: now_unix(),
            what: what.to_string(),
            before,
        });
    }

    pub fn last_fault(&self) -> Option<Fault> {
        self.last_fault.lock().unwrap().clone()
    }

    /// Plain-text diagnostics for the clipboard.
    pub fn diagnostics_report(&self, speakers: bool, versions: bool) -> String {
        let last_error = self.last_error.lock().unwrap().as_ref().map(|(m, _)| m.clone());
        crate::diagnostics::report(
            PRODUCT_NAME,
            versions.then(crate::display_version),
            &self.diagnostics_snapshot(),
            last_error.as_deref(),
            self.last_fault().as_ref(),
            speakers,
        )
    }

    /// The AirPlay buffer used when connecting to `id`.
    pub fn airplay_latency_ms_for(&self, id: &str) -> u32 {
        self.user_config.lock().unwrap().airplay_latency_ms_for(id)
    }

    // -------------------------------------------------------------------
    // Per-speaker settings
    // -------------------------------------------------------------------

    pub fn speaker_settings(&self, id: &str) -> SpeakerSettings {
        self.user_config
            .lock()
            .unwrap()
            .speaker_settings
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Store one speaker's settings (an all-default value drops its entry).
    /// Nothing is stored under a newcomer's id.
    pub fn set_speaker_settings(&self, id: &str, settings: SpeakerSettings) {
        if crate::airplay::discovery::is_instance_id(id) {
            debug!("not storing settings under {id}: it names a receiver announcing another speaker's identity");
            return;
        }
        let mut uc = self.user_config.lock().unwrap();
        let changed = if settings.is_default() {
            uc.speaker_settings.remove(id).is_some()
        } else if uc.speaker_settings.get(id) != Some(&settings) {
            uc.speaker_settings.insert(id.to_string(), settings);
            true
        } else {
            false
        };
        if changed {
            uc.save();
        }
    }

    /// The transport launch planner may never pick a speaker on its own.
    pub fn auto_connect_allowed_at_launch(&self, _id: &str) -> bool {
        false
    }

    /// Every listed speaker as `--player` sees it, in list order.
    pub(crate) fn player_candidates(&self) -> Vec<PlayerCandidate> {
        let rows = self.speaker_view_opts(true).speakers;
        let mut candidates: Vec<PlayerCandidate> = rows
            .iter()
            // A receiver announcing another speaker's identity plays only
            // when a person picks it in the window: `--player` never takes
            // it, nor lets it stand in for the speaker it claims to be.
            .filter(|r| !self.is_airplay_newcomer(&r.id))
            .map(|r| {
                // The names the list shows, without its labels around them
                // ("Kitchen (AirPlay)", "Den (AirPlay, added manually)").
                let mut c = PlayerCandidate {
                    id: r.id.clone(),
                    address: r.ip.clone(),
                    names: Vec::new(),
                    group_rooms: Vec::new(),
                    device_name: None,
                    kind: PlayerKind::Other,
                    manual: r.manual,
                    hidden: r.hidden,
                    used_before: r.manual,
                };
                if r.manual {
                    c.names.extend(self.find_manual_speaker(&r.id).map(|m| m.display_name()));
                } else if let Some(d) = self.discovery.as_ref().and_then(|d| d.find_by_id(&r.id)) {
                    // A Sonos speaker's room is its name too (a group is
                    // labelled by it).
                    let shown = d.display_name();
                    c.names.push(shown.clone());
                    c.names.extend(d.zone_name.clone());
                    c.group_rooms = d.group_members.clone();
                    c.device_name = (d.friendly_name != shown).then(|| d.friendly_name.clone());
                    c.kind = PlayerKind::Upnp;
                } else if let Some(a) = self.airplay_discovery.as_ref().and_then(|d| d.find_by_id(&r.id)) {
                    c.names.push(a.friendly_name);
                    c.kind = PlayerKind::AirPlay;
                } else {
                    c.names.extend(self.speaker_name(&r.id));
                }
                if c.names.is_empty() {
                    c.names.push(r.friendly_name.clone());
                }
                c
            })
            .collect();
        // What the settings already record of a speaker having been used:
        // last time, its own settings, a pairing or a password.
        let uc = self.user_config.lock().unwrap();
        for c in candidates.iter_mut() {
            let settings = uc.speaker_settings.get(&c.id);
            c.used_before |= uc.last_speaker_id.as_deref() == Some(c.id.as_str())
                || settings.is_some()
                || uc.airplay_pairings.contains_key(&c.id)
                || uc.airplay_passwords.contains_key(&c.id);
        }
        candidates
    }

    /// The listed speaker `query` names, ranked as `--player` documents it
    /// (see [`rank_player_match`]), with how it matched.
    pub fn find_listed_speaker(&self, query: &str) -> Option<PlayerMatch> {
        rank_player_match(query, &self.player_candidates())
    }

    /// Where a `--player` wait stands, while there is one.
    pub fn player_wish(&self) -> Option<PlayerWish> {
        self.player_wish.lock().unwrap().clone()
    }

    /// `--player`: connect to the speaker `query` names (see
    /// [`rank_player_match`]) once discovery has had its first look,
    /// unless the person chooses an output first. Each time it appears it is
    /// tried once, without a PIN prompt; the wait ends when a speaker plays
    /// or the person chooses where the sound plays.
    pub fn connect_when_listed(self: &Arc<Self>, query: String) {
        self.connect_when_listed_every(query, Duration::from_secs(1));
    }

    /// [`App::connect_when_listed`], looking every `poll`.
    pub(crate) fn connect_when_listed_every(self: &Arc<Self>, query: String, poll: Duration) {
        let picks_seen = self.user_pick_count();
        info!("--player {:?}: connecting when a speaker by that name or address is found", query);
        *self.player_wish.lock().unwrap() = Some(PlayerWish::Looking { query: query.clone() });
        let named = query.clone();
        let app = self.clone();
        let spawned = std::thread::Builder::new()
            .name("stream-to-speaker-player-wish".into())
            .spawn(move || {
                let mut steps = WishSteps::new(Instant::now());
                // The connect under way: its speaker's id and name, and the
                // last fault before it, so its own failure can be told.
                let mut attempt: Option<(String, String, Option<Fault>)> = None;
                loop {
                    if app.user_pick_count() != picks_seen {
                        info!("{}", player_wait_ended(&query));
                        break;
                    }
                    let busy = app.connecting_to().is_some();
                    if !busy {
                        if let Some((id, name, fault_before)) = attempt.take() {
                            if app.is_speaker_bound() {
                                break;
                            }
                            let fault = app.last_fault().filter(|f| Some(f) != fault_before.as_ref());
                            let reason = fault.map(|f| {
                                f.what.strip_prefix(CONNECT_FAILED_PREFIX).unwrap_or(&f.what).to_string()
                            });
                            info!("--player {:?}: connecting to {:?} failed; waiting for it to come back", query, id);
                            *app.player_wish.lock().unwrap() = Some(PlayerWish::failed(&query, name, reason));
                        }
                    }
                    let rows = app.player_candidates();
                    // The UPnP list is filled once a whole sweep is done.
                    let searched = app.discovery.as_ref().map_or(true, |d| !d.renderers().is_empty());
                    match steps.next(Instant::now(), &query, &rows, searched, busy) {
                        Some(m) => {
                            let name = app.speaker_name(&m.id).unwrap_or_else(|| m.id.clone());
                            if let Some(line) = m.explain(&query, &name) {
                                info!("{}", line);
                            }
                            info!("--player {:?}: found {:?}; connecting", query, m.id);
                            *app.player_wish.lock().unwrap() = Some(PlayerWish::Looking { query: query.clone() });
                            if app.command_sink.get().is_some() {
                                match app.play_and_wait(crate::model::Surface::CommandLine, &m.id, Some(picks_seen)) {
                                    Ok(()) => break,
                                    Err(why) => {
                                        warn!("--player {:?}: {}", query, why);
                                        *app.player_wish.lock().unwrap() = Some(PlayerWish::failed(&query, name, Some(why)));
                                    }
                                }
                            } else {
                                attempt = Some((m.id.clone(), name, app.last_fault()));
                                app.select_speaker_async_full(&m.id, false, false, Some(picks_seen));
                            }
                        }
                        None if steps.tried.is_none() && attempt.is_none() => {
                            // What it tried has gone from the list: looking again.
                            *app.player_wish.lock().unwrap() = Some(PlayerWish::Looking { query: query.clone() });
                        }
                        None => {}
                    }
                    if !app.sleep_unless_shutdown(poll) {
                        break;
                    }
                }
                *app.player_wish.lock().unwrap() = None;
            });
        if let Err(e) = spawned {
            *self.player_wish.lock().unwrap() = None;
            self.record_error(format!("Couldn't start looking for the speaker \"{}\" (--player): {}", named, e));
        }
    }

    /// Keep the Windows output device's name in step with the speaker being
    /// played on: `apply(Some(name))` while one is bound, `apply(None)` (the
    /// plain name) otherwise, at start and whenever that changes, however
    /// the session ended. Looks every second.
    pub fn follow_endpoint_label(self: &Arc<Self>, apply: impl Fn(Option<&str>) + Send + 'static) {
        self.follow_endpoint_label_every(apply, Duration::from_secs(1));
    }

    /// [`App::follow_endpoint_label`], looking every `poll`.
    pub(crate) fn follow_endpoint_label_every(
        self: &Arc<Self>,
        apply: impl Fn(Option<&str>) + Send + 'static,
        poll: Duration,
    ) {
        let app = self.clone();
        let spawned = std::thread::Builder::new().name("stream-to-speaker-device-name".into()).spawn(move || {
            let mut applied: Option<Option<String>> = None;
            loop {
                let now = app.selected_speaker().map(|s| s.friendly_name);
                if applied.as_ref() != Some(&now) {
                    apply(now.as_deref());
                    applied = Some(now);
                }
                if !app.sleep_unless_shutdown(poll) {
                    return;
                }
            }
        });
        if spawned.is_err() {
            warn!("couldn't start keeping the Windows device name in step with the speaker");
        }
    }

    /// Look for Sendspin speakers (when Sendspin is on in the settings)
    /// without offering this PC's sound to Music Assistant: for listing
    /// the speakers while another copy of the app may be running.
    pub fn browse_sendspin_speakers(&self) {
        self.sendspin.discovery_allowed.store(true, Ordering::SeqCst);
        self.sync_sendspin_discovery();
    }

    // -------------------------------------------------------------------
    // Shutdown
    // -------------------------------------------------------------------

    /// Quit: end streaming and tear the session down, waiting only so long
    /// for the speaker. Called once the window or the headless loop has
    /// ended, after [`App::request_shutdown`].
    pub fn shut_down(self: &Arc<Self>) {
        self.shut_down_within(QUIT_CONNECT_WAIT, QUIT_TEARDOWN_WAIT);
    }

    /// [`App::shut_down`] waiting at most `connect_wait` for a connect in
    /// flight and `teardown_wait` for the speaker to answer; a second
    /// Ctrl+C ([`App::quit_without_waiting`]) ends both waits.
    pub(crate) fn shut_down_within(self: &Arc<Self>, connect_wait: Duration, teardown_wait: Duration) {
        // A connect still in flight binds only while streaming is enabled
        // (it checks under the session lock): with it off, it stops what
        // it set up instead of leaving the speaker with an abandoned
        // session. Not saved: the next start streams as usual.
        self.streaming_enabled.store(false, Ordering::Release);
        let end = Instant::now() + connect_wait;
        while self.connecting_to().is_some() && Instant::now() < end && !self.quitting_now() {
            std::thread::sleep(Duration::from_millis(20));
        }
        info!("shutdown: tearing down active session");
        let (done_tx, done) = crossbeam_channel::bounded::<()>(1);
        let app = self.clone();
        let spawned = std::thread::Builder::new().name("stream-to-speaker-quit".into()).spawn(move || {
            app.stop_sendspin();
            let s = app.session.lock().unwrap().take().map(|s| app.releasing(s, None));
            if let Some(s) = s {
                s.stop();
            }
            let _ = done_tx.send(());
        });
        match spawned {
            Ok(_) => {
                // A speaker that answers is told at once; one that has gone
                // isn't waited for: this process holds the single-instance
                // lock until it ends, and a new launch waits for it.
                let end = Instant::now() + teardown_wait;
                loop {
                    match done.recv_timeout(Duration::from_millis(20)) {
                        Err(crossbeam_channel::RecvTimeoutError::Timeout)
                            if Instant::now() < end && !self.quitting_now() => {}
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            warn!("shutdown: the speaker didn't answer in time; quitting without waiting for it");
                            break;
                        }
                        _ => break,
                    }
                }
            }
            Err(_) => {
                self.stop_sendspin();
                let s = self.session.lock().unwrap().take().map(|s| self.releasing(s, None));
                if let Some(s) = s {
                    s.stop();
                }
            }
        }
        info!("shutdown: complete");
    }

    /// A second Ctrl+C: quit without waiting for anything.
    pub fn quit_without_waiting(&self) {
        self.quit_now.store(true, Ordering::SeqCst);
    }

    fn quitting_now(&self) -> bool {
        self.quit_now.load(Ordering::SeqCst)
    }

    /// A speaker picked from the web API: refused once the app is quitting
    /// (it would bind a speaker the app is about to leave, after the quit
    /// switched streaming off for exactly that reason).
    pub fn select_speaker_from_remote(self: &Arc<Self>, id: &str) -> Result<(), String> {
        if self.is_shutting_down() {
            return Err(format!("{} is quitting: no speaker can be chosen now.", PRODUCT_NAME));
        }
        self.select_speaker_from(crate::model::Surface::Api, id)
    }

    /// Run `hook` once, when quitting begins (Quit chosen, the window closed
    /// for good, Ctrl+C).
    pub fn on_quit(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.on_quit_from(move |_| hook());
    }

    pub fn on_quit_from(&self, hook: impl Fn(crate::model::Surface) + Send + Sync + 'static) {
        let _ = self.on_quit.set(Box::new(hook));
    }

    pub fn request_shutdown(&self) {
        self.request_shutdown_from(crate::model::Surface::Panel);
    }

    pub fn request_shutdown_from(&self, from: crate::model::Surface) {
        if !self.shutdown.swap(true, Ordering::SeqCst) {
            if let Some(hook) = self.on_quit.get() {
                hook(from);
            }
            self.quit_hook_finished.store(true, Ordering::Release);
        }
    }

    /// Wait for shutdown, then give the quit hook time to return Windows’ outputs.
    pub fn wait_for_headless_shutdown(&self, hook_timeout: Duration) {
        while !self.is_shutting_down() {
            std::thread::sleep(Duration::from_millis(200));
        }
        let deadline = Instant::now() + hook_timeout;
        while !self.quit_hook_finished.load(Ordering::Acquire) && !self.quitting_now() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() { break; }
            std::thread::sleep(left.min(Duration::from_millis(20)));
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    // -------------------------------------------------------------------
    // The engine's hooks
    // -------------------------------------------------------------------

    /// From now on, today's entry points (the window, the tray, the web
    /// API, the terminal prompt) send their commands to `sink`.
    pub fn set_command_sink(&self, sink: CommandSink) {
        let _ = self.command_sink.set(sink);
    }

    pub(crate) fn engine_view(&self) -> Option<Arc<crate::model::Snapshot>> {
        self.engine_port.get()?.upgrade().map(|port| port.snapshot())
    }

    /// Send `cmd` from `from` to the engine, if it runs. Returns whether
    /// it was sent.
    pub(crate) fn send_command(&self, from: crate::model::Surface, cmd: crate::model::Command) -> bool {
        match self.command_sink.get() {
            Some(sink) => {
                let _ = sink(from, cmd);
                true
            }
            None => false,
        }
    }

    /// Connect to `id` and make it what plays, for the engine, on the
    /// calling thread. A pick in the app's own window may ask for a secret.
    pub(crate) fn engine_connect(self: &Arc<Self>, id: &str, window: bool) -> Result<(), String> {
        *self.connecting.lock().unwrap() = Some(self.speaker_name(id).unwrap_or_else(|| id.to_string()));
        let result = self.select_speaker_opts(id, window, window);
        self.forget_typed_password(id);
        *self.connecting.lock().unwrap() = None;
        self.clear_connecting_notice();
        result
    }

    /// Whether `id` asked for an AirPlay password in this run.
    pub(crate) fn asked_for_password(&self, id: &str) -> bool {
        self.airplay_asked_password.lock().unwrap().contains(id)
    }

    /// A transport event from the bound UPnP speaker (the NOTIFY route).
    pub fn note_transport(&self, change: crate::gena::TransportChange) {
        *self.transport_change.lock().unwrap() = Some(change);
    }

    pub(crate) fn take_transport_change(&self) -> Option<crate::gena::TransportChange> {
        self.transport_change.lock().unwrap().take()
    }

    /// A topology event from the bound Sonos room (the NOTIFY route).
    pub fn note_topology(&self, groups: Vec<crate::sonos::ZoneGroup>) {
        *self.topology.lock().unwrap() = Some(groups);
    }

    pub(crate) fn take_topology(&self) -> Option<Vec<crate::sonos::ZoneGroup>> {
        self.topology.lock().unwrap().take()
    }

    /// Look at speaker `id` before sound plays on it: whether it answers,
    /// and whether it plays something else where it shows that. A UPnP
    /// speaker is asked for its transport and what it plays; an AirPlay
    /// receiver's announcement says whether another device sends to it,
    /// and otherwise a connection to its control port (with nothing sent
    /// on it) says whether it answers. Other speakers show nothing: free.
    pub fn look_at_speaker(&self, id: &str) -> crate::engine::signals::SpeakerSignal {
        use crate::engine::signals::{answers_on, SpeakerSignal, STILL_THERE_WAIT};
        let _asking = crate::engine::cancel::asking_speaker();
        if let Some(r) = self.discovery.as_ref().and_then(|d| d.find_by_id(id)) {
            let Ok(local) = self.local_ip_toward(r.ip) else { return SpeakerSignal::NotResponding };
            let ours = format!("http://{}/", SocketAddr::new(local, self.config.bind.port()));
            let Ok(state) = upnp::get_transport_state(&r.av_transport_control_url) else {
                return SpeakerSignal::NotResponding;
            };
            if crate::engine::cancel::cancelled() { return SpeakerSignal::NotResponding; }
            let Ok((uri, title)) = upnp::get_media_info(&r.av_transport_control_url) else {
                return SpeakerSignal::NotResponding;
            };
            return if upnp::is_busy(&state, &uri, &ours) {
                SpeakerSignal::PlayingElse { who: title.as_deref().and_then(upnp::other_pc_in_title) }
            } else {
                SpeakerSignal::Free
            };
        }
        if let Some(r) = self.find_airplay_renderer(id) {
            if self.another_device_sends_to(&r) {
                return SpeakerSignal::PlayingElse { who: None };
            }
            let port = if r.port != 0 { r.port } else { r.airplay_port.unwrap_or(DEFAULT_AIRPLAY_PORT) };
            return if answers_on(SocketAddr::new(r.ip, port), STILL_THERE_WAIT) {
                crate::engine::cancel::speaker_answered();
                SpeakerSignal::Free
            } else {
                SpeakerSignal::NotResponding
            };
        }
        SpeakerSignal::Free
    }

    /// Whether `r`'s announcement says another device sends to it, unless
    /// this PC played there within the last minute (its own session may
    /// still be announced for a moment after it ended).
    pub(crate) fn another_device_sends_to(&self, r: &crate::airplay::AirPlayRenderer) -> bool {
        const OWN_SESSION_ANNOUNCED_FOR: Duration = Duration::from_secs(60);
        let ours_lately = self
            .last_ended
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(id, at)| *id == r.stable_id() && at.elapsed() < OWN_SESSION_ANNOUNCED_FOR);
        let ours_now = self.session.lock().unwrap().as_ref().is_some_and(|s| s.stable_id() == r.stable_id());
        !ours_lately && !ours_now && r.another_device_sending()
    }

    /// Take the stream off the bound speaker after a long quiet, where the
    /// speaker lets that happen, so that it is free to others meanwhile:
    /// a UPnP speaker is told `Stop` and its session kept for
    /// [`Self::restart_upnp_stream_number`]; an AirPlay receiver is made silent
    /// and let go (connected again when sound starts). Others keep their
    /// stream. What was done; None for nothing.
    pub(crate) fn take_stream_off_number(self: &Arc<Self>, number: Option<u64>) -> Option<TakenOff> {
        let (avt, name, _device) = {
            let guard = self.session.lock().unwrap();
            if number.is_some_and(|n| n != self.session_number()) {
                return None;
            }
            let session = guard.as_ref()?;
            match session {
                ActiveSession::Upnp(r) => (r.renderer.av_transport_control_url.clone(), r.renderer.friendly_name.clone(), self.stopping_device(session, None)),
                ActiveSession::AirPlay(_) | ActiveSession::AirPlay2(_) => {
                    let id = guard.as_ref()?.stable_id();
                    let number = self.session_number();
                    drop(guard);
                    if let Some(h) = self.release_number_then(Some(number), true, Box::new(|| {}), None) {
                        let _ = h.join();
                    }
                    info!("took the stream off {} after a long quiet", id);
                    return Some(TakenOff::LetGo(id));
                }
                ActiveSession::Sendspin(_) | ActiveSession::MusicAssistant(_) => return None,
            }
        };
        if let Err(e) = upnp::stop(&avt) {
            warn!("UPnP Stop on {} failed: {}", name, e);
            return None;
        }
        info!("took the stream off {} after a long quiet", name);
        Some(TakenOff::Stopped)
    }

    /// Give the bound UPnP speaker its stream again from the start: the
    /// same address with its metadata, then Play, so that nothing it held
    /// from before (a long pause, the stream taken off) plays first.
    pub(crate) fn restart_upnp_stream_number(&self, expected: Option<u64>) -> Result<(), String> {
        let (avt, uri, profile, number) = {
            let guard = self.session.lock().unwrap();
            let Some(ActiveSession::Upnp(r)) = guard.as_ref() else {
                return Err("not playing on a UPnP speaker".into());
            };
            let number = self.session_number();
            if expected.is_some_and(|n| n != number) || crate::engine::cancel::cancelled() {
                return Err(SUPERSEDED.into());
            }
            if let Some(job) = crate::engine::cancel::binding() {
                job.number.store(number, Ordering::Release);
            }
            (r.renderer.av_transport_control_url.clone(), r.stream_uri.clone(), r.profile, number)
        };
        let didl = upnp::didl_lite_metadata(&uri, PRODUCT_NAME, self.config.initial_buffer_ms, profile);
        let given_at = Instant::now();
        let _asking = crate::engine::cancel::asking_speaker();
        upnp::set_av_transport_uri(&avt, &uri, &didl, &upnp::TRANSPORT).map_err(|e| format!("{e:#}"))?;
        if crate::engine::cancel::cancelled() || self.session_number() != number {
            return Err(SUPERSEDED.into());
        }
        upnp::play(&avt).map_err(|e| format!("{e:#}"))?;
        let mut guard = self.session.lock().unwrap();
        if let Some(ActiveSession::Upnp(r)) = guard.as_mut().filter(|_| self.session_number() == number) {
            r.stream_given_at = given_at;
            r.played_at = Instant::now();
        }
        Ok(())
    }

    /// Read under the session lock when identifying a particular binding.
    pub(crate) fn session_number(&self) -> u64 {
        self.speaker_control.session_number()
    }

    /// Reserve the device before Play so another connect waits through a cancelled start's Stop.
    fn wait_for_release(&self, id: &str, renderer: Option<&Renderer>) -> Result<DeviceUse, String> {
        let speaker = crate::engine::cancel::binding().map(|j| j.speaker.clone());
        let devices = self.device_uses.devices.lock().unwrap();
        let (mut devices, timeout) = self.device_uses.ended.wait_timeout_while(devices, Duration::from_secs(8), |devices| {
            devices.iter().any(|d| d.id == id
                || speaker.is_some() && d.speaker == speaker
                || d.renderer.as_ref().zip(renderer).is_some_and(|(a, b)| a.same_device_as(b)))
        }).unwrap();
        if timeout.timed_out() { return Err(format!("Timed out waiting for the previous session on {id} to stop.")); }
        check_job()?;
        Ok(self.device_uses.register(&mut devices, SessionDevice { id: id.to_string(), speaker, renderer: renderer.cloned() }))
    }

    fn stopping_device(&self, session: &ActiveSession, speaker: Option<crate::model::SpeakerId>) -> DeviceUse {
        let renderer = match session { ActiveSession::Upnp(s) => Some(s.renderer.clone()), _ => None };
        let device = SessionDevice { id: session.stable_id(), speaker, renderer };
        self.device_uses.register(&mut self.device_uses.devices.lock().unwrap(), device)
    }

    /// Record the release while the session lock still identifies the binding.
    pub(crate) fn releasing(&self, session: ActiveSession, speaker: Option<crate::model::SpeakerId>) -> ReleasingSession {
        let device = self.stopping_device(&session, speaker);
        ReleasingSession { session, device }
    }

    /// Cancel the connect under the same lock it uses to bind. Report whether it
    /// owned the session we took, even if its stop has to run on this thread.
    pub(crate) fn release_job(self: &Arc<Self>, job: &crate::engine::cancel::Binding) -> (bool, Option<std::thread::JoinHandle<()>>) {
        let session = {
            let mut guard = self.session.lock().unwrap();
            job.cancelled.store(true, Ordering::SeqCst);
            let number = job.number.load(Ordering::Acquire);
            if number != 0 && number == self.session_number() { guard.take().map(|s| self.releasing(s, Some(job.speaker.clone()))) } else { None }
        };
        match session {
            Some(session) => (true, self.release_taken(session, true, Box::new(|| {}))),
            None => (false, None),
        }
    }

    /// A later session, even to the same speaker, belongs to another job.
    pub(crate) fn release_number_then(
        self: &Arc<Self>, number: Option<u64>, tell_speaker: bool, then: Box<dyn FnOnce() + Send>, job: Option<&crate::engine::cancel::Binding>,
    ) -> Option<std::thread::JoinHandle<()>> {
        self.release_speaker_number_then(number, tell_speaker, then, job, job.map(|j| j.speaker.clone()))
    }

    /// [`Self::release_number_then`] for the session of `speaker`: while the
    /// release tells the speaker to stop (a UPnP Stop, an AirPlay TEARDOWN),
    /// a connect to that speaker by its other way waits for it too.
    pub(crate) fn release_speaker_number_then(
        self: &Arc<Self>, number: Option<u64>, tell_speaker: bool, then: Box<dyn FnOnce() + Send>,
        job: Option<&crate::engine::cancel::Binding>, speaker: Option<crate::model::SpeakerId>,
    ) -> Option<std::thread::JoinHandle<()>> {
        let session = {
            let mut guard = self.session.lock().unwrap();
            if number.is_none_or(|n| n == self.session_number()) && !job.is_some_and(|j| j.cancelled.load(Ordering::SeqCst)) {
                take_for_connect(&mut guard, self.session_number(), job).map(|s| {
                    // A UPnP session let go without telling the speaker is only unsubscribed.
                    let told = tell_speaker || !matches!(s, ActiveSession::Upnp(_));
                    self.releasing(s, speaker.filter(|_| told))
                })
            } else { None }
        };
        let Some(session) = session else {
            then();
            return None;
        };
        self.release_taken(session, tell_speaker, then)
    }

    fn release_taken(self: &Arc<Self>, session: ReleasingSession, tell_speaker: bool, then: Box<dyn FnOnce() + Send>) -> Option<std::thread::JoinHandle<()>> {
        *self.last_ended.lock().unwrap() = Some((session.session.stable_id(), Instant::now()));
        self.prune_stream_clients();
        let cell = Arc::new(Mutex::new(Some((session, then))));
        let app = self.clone();
        let stop = move |cell: &Mutex<Option<(ReleasingSession, Box<dyn FnOnce() + Send>)>>| {
            let Some((ReleasingSession { session, device }, then)) = cell.lock().unwrap().take() else { return };
            match session {
                ActiveSession::Upnp(s) if !tell_speaker => unsubscribe_all(&s),
                other if tell_speaker => {
                    let (name, id) = (other.friendly_name(), other.stable_id());
                    if other.silence() {
                        debug!("{} took the stop", name);
                        app.signals.push(&id, crate::engine::signals::SpeakerSignal::Silenced);
                    } else {
                        info!("{} didn't answer the stop; it plays out what it holds", name);
                    }
                }
                other => other.stop(),
            }
            drop(device);
            then();
        };
        let stop = Arc::new(stop);
        let (worker_cell, worker_stop) = (cell.clone(), stop.clone());
        match std::thread::Builder::new()
            .name("stream-to-speaker-release".into())
            .spawn(move || worker_stop(&worker_cell))
        {
            Ok(handle) => Some(handle),
            Err(e) => {
                // No thread to spare: let it go here instead.
                warn!("couldn't start the thread that lets the speaker go ({}); letting it go now", e);
                stop(&cell);
                None
            }
        }
    }

    // -------------------------------------------------------------------
    // Test sound, refused devices, Lab, Music Assistant's level, limits
    // -------------------------------------------------------------------

    /// Play the test sound on whatever plays now: two short notes mixed
    /// into the stream.
    pub fn play_test_sound(&self) {
        let number = self.session.lock().unwrap().as_ref().map(|_| self.session_number());
        if let Some(number) = number { self.play_test_sound_on(number); }
    }

    pub(crate) fn play_test_sound_on(&self, number: u64) {
        *self.test_sound.lock().unwrap() = Some((number, 0));
    }

    pub(crate) fn stop_test_sound_on(&self, number: u64) {
        self.test_sound.lock().unwrap().take_if(|(session, _)| *session == number);
    }

    /// Whether the test sound is still playing.
    pub fn test_sound_playing(&self) -> bool {
        self.test_sound.lock().unwrap().is_some()
    }

    /// Mix the test sound into `samples` (interleaved, `channels` per
    /// frame), if one is playing.
    pub(crate) fn mix_test_sound(&self, samples: &mut [i16], channels: u16) {
        let mut guard = self.test_sound.lock().unwrap();
        let Some((number, pos)) = guard.as_mut() else { return };
        // Keep the session while mixing. If it is changing, leave the chime
        // at this frame for the next packet.
        let session = match self.session.try_lock() {
            Ok(session) => session,
            Err(std::sync::TryLockError::WouldBlock) => return,
            Err(std::sync::TryLockError::Poisoned(_)) => { *guard = None; return; }
        };
        if session.is_none() || self.session_number() != *number {
            *guard = None;
            return;
        }
        let ch = channels.max(1) as usize;
        for frame in samples.chunks_mut(ch) {
            if *pos >= CHIME_FRAMES {
                break;
            }
            let v = chime_at(*pos);
            for s in frame.iter_mut() {
                *s = (*s as i32 + v as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
            *pos += 1;
        }
        if *pos >= CHIME_FRAMES {
            *guard = None;
        }
    }

    fn set_chime_level(&self, id: &str, asked: Option<u8>, control: &VolumeControl, current: impl FnOnce() -> Result<u32>) -> Result<(), String> {
        let limit = self.volume_limit(id);
        let level = match (asked, limit) {
            (Some(level), _) => level.min(limit.unwrap_or(100)),
            (None, Some(limit)) if current().map_err(|e| format!("{e:#}"))? > u32::from(limit) => limit,
            _ => return Ok(()),
        };
        check_job()?;
        control.set_volume_pct(u32::from(level)).map_err(|e| format!("{e:#}"))
    }

    /// Play the test chime on speaker `id`, which isn't the place, while
    /// the place's sound goes on: a second, short session of its own,
    /// ended after the chime. A UPnP speaker is given the chime's file at
    /// a path of the stream server of its own ([`CHIME_PATH`]) and Play;
    /// an AirPlay receiver gets a session of its own that sends the chime
    /// and then lets go. A speaker that plays something else is left
    /// alone (it gets the chime only once a person confirmed that it may
    /// stop what plays there). Runs on the caller's thread: network I/O.
    pub(crate) fn chime_beside(self: &Arc<Self>, id: &str, confirmed: bool, level: Option<u8>, in_window: bool) -> Result<(), String> {
        check_job()?;
        let _asking = crate::engine::cancel::asking_speaker();
        let name = self.speaker_name(id).unwrap_or_else(|| id.to_string());
        if !confirmed && matches!(self.look_at_speaker(id), crate::engine::signals::SpeakerSignal::PlayingElse { .. }) {
            return Err(format!("{name} is playing something else"));
        }
        check_job()?;
        let upnp = self.discovery.as_ref().and_then(|d| d.find_by_id(id)).map(|r| (r.stable_id(), r)).or_else(|| {
            self.find_manual_speaker(id).filter(|m| m.kind == ManualKind::Upnp)
                .and_then(|m| self.manual_renderer(&m).map(|r| (m.id(), r)))
        });
        if let Some((speaker, r)) = upnp {
            let _device = self.wait_for_release(&speaker, Some(&r))?;
            let local = self.local_ip_toward(r.ip)?;
            let uri = format!("http://{}{}", SocketAddr::new(local, self.config.bind.port()), CHIME_PATH);
            let didl = chime_didl(&uri);
            let avt = &r.av_transport_control_url;
            self.set_chime_level(&speaker, level, &VolumeControl::upnp(&r), || match r.group_volume_control_url() {
                Some(url) => upnp::get_group_volume(url),
                None => upnp::get_volume(&r.rendering_control_control_url),
            })?;
            check_job()?;
            upnp::set_av_transport_uri(avt, &uri, &didl, &upnp::TRANSPORT).map_err(|e| format!("{e:#}"))?;
            check_job()?;
            upnp::play(avt).map_err(|e| format!("{e:#}"))?;
            info!("test chime on {name} beside the place");
            return Ok(());
        }
        let manual = self.find_manual_speaker(id).filter(|m| m.kind == ManualKind::AirPlay);
        if self.find_airplay_renderer(id).is_none() && manual.is_none() {
            return Err(format!("{name} can't play a test chime"));
        }
        let hub = crate::http_server::StreamHub::new();
        let (session, _device) = chime_airplay(in_window, || self.start_airplay_from(id, manual.as_ref(), &hub),
            |id, name| self.pair_for_job(id, name))?;
        if let Err(e) = check_job() {
            session.silence();
            return Err(e);
        }
        if let Some(control) = session.volume_control() {
            let result = self.set_chime_level(&session.stable_id(), level, &control, || {
                session.level_on_connect().ok_or_else(|| anyhow!("the speaker didn't give its volume"))
            });
            if let Err(e) = result {
                if crate::engine::cancel::cancelled() { session.silence(); } else { session.stop(); }
                return Err(e);
            }
        }
        // The chime in the frames the audio loop sends, at their pace,
        // then silence while the receiver plays out what it holds.
        // What a receiver holds before it plays (its usual AirPlay buffer),
        // and a little more.
        let hold = Duration::from_millis(crate::user_config::AIRPLAY_LATENCY_MS_DEFAULT as u64 + 500);
        let frames_per_packet = WIRE_SAMPLE_RATE as usize / 100;
        let chime = chime_pcm(crate::WIRE_CHANNELS);
        let packets = chime.chunks(frames_per_packet * crate::WIRE_CHANNELS as usize);
        let silence = vec![0i16; frames_per_packet * crate::WIRE_CHANNELS as usize];
        let tail = hold.as_millis() as usize / 10;
        let start = Instant::now();
        for (n, samples) in packets.chain(std::iter::repeat(silence.as_slice()).take(tail)).enumerate() {
            if let Err(e) = check_job() {
                session.silence();
                return Err(e);
            }
            hub.publish(crate::http_server::PcmFrame(Arc::new(crate::http_server::samples_to_wav_le_bytes(samples))));
            let due = start + Duration::from_millis(10 * (n as u64 + 1));
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        if let Err(e) = check_job() {
            session.silence();
            return Err(e);
        }
        session.stop();
        check_job()?;
        info!("test chime on {name} beside the place");
        Ok(())
    }

    fn note_refused(&self, peer: IpAddr) {
        let peer = canonical_ip(peer);
        let name = self.speaker_name_at(peer).unwrap_or_else(|| peer.to_string());
        // A session gets its number under this lock.
        let (speaker, session) = {
            let bound = self.session.lock().unwrap();
            (bound.as_ref().map(|s| s.stable_id()), self.session_number())
        };
        let mut list = self.refused.lock().unwrap();
        list.retain(|r| r.address != peer);
        list.insert(0, RefusedDevice { speaker, session, address: peer, name, at: std::time::SystemTime::now() });
        list.truncate(REFUSALS_KEPT);
    }

    /// The devices refused the stream lately, newest first.
    pub fn recently_refused(&self) -> Vec<RefusedDevice> {
        self.refused.lock().unwrap().clone()
    }

    /// Let `address` fetch the stream for the rest of this run, whatever
    /// privacy mode says.
    pub fn allow_device(&self, address: IpAddr) {
        let address = canonical_ip(address);
        self.refused.lock().unwrap().retain(|r| r.address != address);
        let mut allowed = self.allowed.lock().unwrap();
        if !allowed.iter().any(|(a, _)| *a == address) {
            info!("{} may fetch the stream for this run", address);
            allowed.push((address, self.stream_gate.grant([address])));
        }
    }

    /// Forget the refusals shown (the notice was dismissed).
    pub fn dismiss_refusals(&self) {
        self.refused.lock().unwrap().clear();
    }

    /// A speaker's name by its address, when discovery knows it.
    fn speaker_name_at(&self, peer: IpAddr) -> Option<String> {
        let upnp = self.discovery.as_ref().and_then(|d| {
            d.renderers().into_iter().find(|r| r.stream_peers().iter().any(|ip| canonical_ip(*ip) == peer)).map(|r| r.display_name())
        });
        upnp.or_else(|| {
            self.airplay_discovery
                .as_ref()
                .and_then(|d| d.renderers().into_iter().find(|r| canonical_ip(r.ip) == peer).map(|r| r.friendly_name))
        })
    }

    /// Whether any Lab setting is away from its usual value.
    pub fn lab_on(&self) -> bool {
        let uc = self.user_config.lock().unwrap();
        self.rate_fudge_ppm.load(Ordering::Relaxed) != 0
            || self.silence_pace_ms.load(Ordering::Relaxed) != 10
            || self.latency_adjust_step_frames.load(Ordering::Relaxed) != 4
            || uc.delay_at_once
            || uc.prefer_realtime_airplay
            || uc.airplay_mfi_encryption
            || uc.airplay_uncompressed_alac
    }

    /// Every Lab setting back to its usual value.
    pub fn turn_off_lab(&self) {
        self.rate_fudge_ppm.store(0, Ordering::Relaxed);
        self.silence_pace_ms.store(10, Ordering::Relaxed);
        self.latency_adjust_step_frames.store(4, Ordering::Relaxed);
        self.delay.catch_up().set_at_once(false);
        let mut uc = self.user_config.lock().unwrap();
        uc.delay_at_once = false;
        uc.prefer_realtime_airplay = false;
        uc.airplay_mfi_encryption = false;
        uc.airplay_uncompressed_alac = false;
        uc.save();
    }

    /// The level this PC sends to Music Assistant (0–100).
    pub fn ma_level(&self) -> u8 {
        self.sendspin.ma_level.load(Ordering::Relaxed)
    }

    /// Set the level this PC sends to Music Assistant; remembered.
    pub fn set_ma_level(&self, level: u8) {
        let level = level.min(100);
        self.sendspin.ma_level.store(level, Ordering::Relaxed);
        let mut uc = self.user_config.lock().unwrap();
        if uc.ma_level != Some(level) {
            uc.ma_level = Some(level);
            uc.save();
        }
    }

    /// A speaker's *Never louder than*, if set below 100.
    pub fn volume_limit(&self, id: &str) -> Option<u8> {
        self.speaker_settings(id).volume_limit.filter(|l| *l < 100)
    }

    /// Apply a newly saved maximum to the session that owns it.
    pub(crate) fn apply_bound_volume_limit(&self, edited: &[String]) {
        let bound = self.session.lock().unwrap();
        let Some(session) = bound.as_ref() else { return };
        let id = session.stable_id();
        if !edited.contains(&id) { return; }
        let Some(limit) = self.volume_limit(&id) else { return };
        if matches!(session, ActiveSession::MusicAssistant(_)) {
            if self.ma_level() > limit { self.set_ma_level(limit); }
        } else if self.current_volume().is_some_and(|v| v > u32::from(limit)) {
            let level = u32::from(limit);
            self.speaker_control.send(SpeakerCommand::Volume(level));
            if self.vsync.window_changed(level) {
                drop(bound);
                push_windows_level(&self.windows_volume, level, self.vsync.is_muted());
            }
        }
    }

    /// The bound speaker's limit.
    fn bound_limit(&self) -> Option<u8> {
        let id = self.session.lock().unwrap().as_ref().map(|s| s.stable_id())?;
        self.volume_limit(&id)
    }


}

// -----------------------------------------------------------------------------
// Session lifecycle helpers
// -----------------------------------------------------------------------------

/// How long after Play a renderer may take to come for the stream before
/// that is worth a warning.
const FETCH_EXPECTED_WITHIN: Duration = Duration::from_secs(10);

/// How long a connect may take: every offer, retry and question to the
/// speaker together. Each action's own limit (30 s for SetAVTransportURI
/// and Play) still applies; the budget only takes the smaller.
const CONNECT_BUDGET: Duration = Duration::from_secs(45);

/// Budget a new offer needs to start. A speaker that turns an offer down
/// at once does so in well under a second, so this only stops an offer
/// after slow ones: with less left, a speaker that answers in a few
/// seconds per action would be cut off by the budget anyway, and the
/// person would be told nothing new.
const MIN_LEFT_FOR_OFFER: Duration = Duration::from_secs(10);

/// Budget the second Play after a 701 needs: half a second's pause, then
/// a Play that a renderer which has finished taking the URI answers at
/// once.
const MIN_LEFT_FOR_RETRY: Duration = Duration::from_secs(5);

/// An action with less budget than this is not sent: no answer could
/// arrive in time.
const MIN_LEFT_FOR_ACTION: Duration = Duration::from_millis(500);

/// How long the speaker may keep the person waiting before the banner
/// says it is waiting for the speaker.
const WAITING_NOTICE_AFTER: Duration = Duration::from_secs(3);

/// What a renderer is offered: this PC's stream address on its network
/// (`http://<ip>:<port>`), the stream profile it played last time, if
/// any, and how long the connect may take ([`CONNECT_BUDGET`] in the
/// app). `initial_buffer_ms` (`--initial-buffer-ms`) reaches the metadata
/// builder, which leaves it out: no renderer is sent a buffer hint.
pub struct StreamOffer {
    pub base_url: String,
    pub remembered: Option<StreamProfile>,
    pub initial_buffer_ms: u32,
    pub budget: Duration,
}

/// Why a renderer didn't start: its last answer, after this many stream
/// profiles were offered. `out_of_time`: the connect's budget (this long)
/// ran out before the speaker had answered.
#[derive(Debug)]
pub struct StartError {
    pub error: anyhow::Error,
    pub formats_tried: usize,
    pub out_of_time: Option<Duration>,
}

/// Where a connect is, for the banner: which offer (0 is the first), and
/// whether it has been waiting more than [`WAITING_NOTICE_AFTER`] for the
/// speaker's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectStep {
    pub offer: usize,
    pub waiting: bool,
}

impl ConnectStep {
    /// The banner's line under "Connecting to …", or None for the usual
    /// one.
    pub fn text(self) -> Option<String> {
        let way = if self.offer >= 2 { "a third way" } else { "another way" };
        match (self.offer, self.waiting) {
            (0, false) => None,
            (0, true) => Some("Waiting for the speaker to answer…".into()),
            (1, false) => Some("The speaker didn't take the stream. Offering it another way…".into()),
            (_, false) => Some("The speaker didn't take that either. Offering the stream a third way…".into()),
            (_, true) => Some(format!("Offering the stream {way}. Waiting for the speaker to answer…")),
        }
    }
}

/// The time a connect has left.
struct Budget {
    until: Instant,
}

impl Budget {
    /// Time left for the connect: none once a newer choice superseded it,
    /// so it stops before its next request.
    fn left(&self) -> Duration {
        if crate::engine::cancel::cancelled() {
            return Duration::ZERO;
        }
        self.until.saturating_duration_since(Instant::now())
    }

    /// `own`, shortened to what is left; None when too little is left to
    /// send the action at all.
    fn limits(&self, own: &crate::device_http::Limits) -> Option<crate::device_http::Limits> {
        let left = self.left();
        (left >= MIN_LEFT_FOR_ACTION).then(|| own.within(left))
    }
}

/// The connect's budget ran out before an action could be sent.
#[derive(Debug)]
struct OutOfTime(&'static str);

impl std::fmt::Display for OutOfTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: no time left to send it", self.0)
    }
}

impl std::error::Error for OutOfTime {}

/// Run one action of a connect. If the speaker keeps it waiting longer
/// than [`WAITING_NOTICE_AFTER`], `progress` hears so, and hears again
/// once the answer is in.
fn waiting_on<T>(progress: &(dyn Fn(ConnectStep) + Sync), offer: usize, action: impl FnOnce() -> T) -> T {
    let (done, finished) = crossbeam_channel::bounded::<()>(0);
    std::thread::scope(|scope| {
        let watcher = std::thread::Builder::new().name("stream-to-speaker-connect-notice".into()).spawn_scoped(scope, move || {
            let late = finished.recv_timeout(WAITING_NOTICE_AFTER) == Err(crossbeam_channel::RecvTimeoutError::Timeout);
            if late {
                progress(ConnectStep { offer, waiting: true });
            }
            late
        });
        let result = action();
        drop(done);
        if watcher.ok().and_then(|w| w.join().ok()).unwrap_or(false) {
            progress(ConnectStep { offer, waiting: false });
        }
        result
    })
}

/// A started session, and what the speaker's settings should remember
/// from now on (see [`stream_profile::to_remember`]).
pub struct Started {
    pub session: RendererSession,
    pub remember: Option<StreamProfile>,
}

/// What a failed offer says about the profile offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// Refused for its content: SetAVTransportURI with 714 (Illegal
    /// MIME-type) or 716 (Resource not found); Play with 702 (No
    /// contents), 703 (Read error), 704 (Format not supported for
    /// playback), 714 or 716. Another profile may be taken.
    Content,
    /// Play refused without saying why (no UPnP code, or the generic
    /// 501): another profile may be taken, but this says too little to
    /// move the speaker off the original stream for good.
    Unclear,
    /// Anything else ends the connect: no answer (another profile won't
    /// change that), the transport's state (701 after its retry, 705,
    /// 715, 718: busy, locked, not ready), other codes.
    End,
}

fn verdict(e: &anyhow::Error, at_play: bool) -> Verdict {
    let Some(a) = e.downcast_ref::<upnp::ActionError>() else { return Verdict::End };
    if !matches!(a.failure, upnp::Failure::Refused { .. }) {
        return Verdict::End;
    }
    match (at_play, a.fault_code()) {
        (false, Some(714 | 716)) => Verdict::Content,
        (true, Some(702 | 703 | 704 | 714 | 716)) => Verdict::Content,
        (true, None | Some(501)) => Verdict::Unclear,
        _ => Verdict::End,
    }
}

/// Start `renderer` playing the stream: each profile in turn (see
/// [`stream_profile::Offers`]) gets a clean Stop, SetAVTransportURI and
/// Play, until one is taken; a refusal of the profile moves on to the
/// next, anything else ends the attempt. The renderer is asked which types
/// it plays only when an offer depends on that. Then subscribe to its
/// volume events.
pub fn start_session(
    renderer: Renderer,
    offer: &StreamOffer,
    callback_url: &str,
    stream_grant: Option<Grant>,
    progress: &(dyn Fn(ConnectStep) + Sync),
) -> Result<Started, StartError> {
    let name = renderer.display_name();
    info!("targeting speaker: {} ({})", name, renderer.ip);
    let budget = Budget { until: Instant::now() + offer.budget };
    let avt = renderer.av_transport_control_url.as_str();
    let ask = || match (&renderer.connection_manager_control_url, budget.limits(&upnp::PROTOCOL_LIST)) {
        (Some(url), Some(limits)) => match upnp::get_protocol_info(url, &limits) {
            Ok(list) => Sinks::Listed(list),
            Err(e) => {
                debug!("UPnP: {}: no list of the types it plays: {:#}", name, e);
                Sinks::Unknown
            }
        },
        _ => Sinks::Unknown,
    };
    let mut offers = stream_profile::Offers::new(renderer.is_sonos(), offer.remembered);
    let mut current = offers.get(0, &ask);
    if let Some((profile, why)) = current {
        info!("UPnP: {}: offering {}, because {}", name, profile, why);
    }
    let mut taken = None;
    // The original stream refused for its content in this connect: the
    // only reason to remember another profile.
    let mut original_refused = false;
    let mut i = 0;
    while let Some((profile, _)) = current {
        let uri = format!("{}{}", offer.base_url, profile.path());
        let didl = upnp::didl_lite_metadata(&uri, PRODUCT_NAME, offer.initial_buffer_ms, profile);
        // Stop first to clear any "Transport Locked" (Sonos 705)
        // carry-over, and what an offer before this one left.
        if let Some(limits) = budget.limits(&upnp::SHORT) {
            let _ = waiting_on(progress, i, || upnp::stop_within(avt, &limits));
        }
        std::thread::sleep(Duration::from_millis(100));
        let given_at = Instant::now();
        let mut at_play = false;
        let started = (|| {
            let limits = budget.limits(&upnp::TRANSPORT).ok_or(OutOfTime("SetAVTransportURI"))?;
            waiting_on(progress, i, || upnp::set_av_transport_uri(avt, &uri, &didl, &limits))?;
            at_play = true;
            std::thread::sleep(Duration::from_millis(100));
            play_taken_uri(avt, &budget, progress, i)
        })();
        let e = match started {
            Ok(()) => {
                info!("UPnP: {} took {}", name, profile);
                taken = Some((profile, uri, given_at, Instant::now()));
                break;
            }
            Err(e) => e,
        };
        let out_of_time = (e.downcast_ref::<OutOfTime>().is_some()
            || (budget.left() < MIN_LEFT_FOR_ACTION && no_answer_in_time(&e)))
        .then_some(offer.budget);
        let verdict = verdict(&e, at_play);
        if profile == StreamProfile::Legacy && verdict == Verdict::Content {
            original_refused = true;
        }
        let moving_on = verdict != Verdict::End && offers.has(i + 1) && budget.left() >= MIN_LEFT_FOR_OFFER;
        let next = if moving_on {
            progress(ConnectStep { offer: i + 1, waiting: false });
            waiting_on(progress, i + 1, || offers.get(i + 1, &ask))
        } else {
            None
        };
        match next {
            Some((next, why)) => {
                info!("UPnP: {} didn't take {}: {:#}; offering {}, because {}", name, profile, e, next, why);
                current = Some((next, why));
                i += 1;
            }
            None => {
                // The renderer may hold our URI and still start on it:
                // tell it to let go, so a failure reported here is not a
                // speaker playing. Best-effort, with Stop's own limit:
                // this is cleaning up, not a step of the connect.
                if at_play {
                    let _ = upnp::stop(avt);
                }
                // The speaker's own answer, in full, for whoever reads the
                // log.
                warn!("UPnP: {} ({}): {:#}", name, renderer.ip, e);
                return Err(StartError { error: e, formats_tried: i + 1, out_of_time });
            }
        }
    }
    let Some((profile, stream_uri, stream_given_at, played_at)) = taken else {
        return Err(StartError { error: anyhow!("no stream profile to offer"), formats_tried: 0, out_of_time: None });
    };

    // Volume events, with SUBSCRIBE's own limit: the speaker is playing,
    // and a subscription cut short by the budget would leave the whole
    // session without volume sync.
    let gena = GenaManager::new(callback_url.to_string());
    // Groups subscribe to GroupRenderingControl (GroupVolume events —
    // the group slider), standalone speakers to RenderingControl.
    match gena.subscribe(renderer.volume_event_url()) {
        Ok(_) => {
            gena.clone().spawn_renewer();
        }
        Err(e) => {
            warn!("GENA subscribe failed (continuing without volume sync): {}", e);
        }
    }

    // Transport events: whether the speaker re-buffers, or plays something
    // else now that someone else has taken it.
    let transport = renderer.av_transport_event_url.clone().and_then(|url| {
        let t = GenaManager::new(callback_url.to_string());
        match t.subscribe(&url) {
            Ok(_) => {
                t.clone().spawn_renewer();
                Some(t)
            }
            Err(e) => {
                debug!("transport events unavailable from {}: {}", renderer.friendly_name, e);
                None
            }
        }
    });

    // A Sonos room's topology events: rooms that join its group, or the
    // room taken into another room's group.
    let topology = renderer
        .zone_group_topology_control_url
        .as_deref()
        .filter(|_| renderer.is_sonos())
        .and_then(crate::sonos::topology_event_url)
        .and_then(|url| {
            let t = GenaManager::new(callback_url.to_string());
            match t.subscribe(&url) {
                Ok(_) => {
                    t.clone().spawn_renewer();
                    Some(t)
                }
                Err(e) => {
                    debug!("topology events unavailable from {}: {}", renderer.friendly_name, e);
                    None
                }
            }
        });

    let session = RendererSession {
        renderer,
        gena,
        transport,
        topology,
        stream_uri,
        stream_grant,
        id_override: None,
        profile,
        stream_given_at,
        played_at,
    };
    Ok(Started { session, remember: stream_profile::to_remember(offer.remembered, profile, original_refused) })
}

/// How long a renderer that is still taking the URI gets before Play is
/// sent again.
const PLAY_RETRY_WAIT: Duration = Duration::from_millis(500);

/// Play, right after the renderer accepted our URI, within the connect's
/// budget. A renderer still busy taking it answers 701 ("Transition not
/// available"), so that answer gets Play once more after a moment, when
/// the budget leaves room for it.
fn play_taken_uri(avt: &str, budget: &Budget, progress: &(dyn Fn(ConnectStep) + Sync), offer: usize) -> Result<()> {
    let limits = budget.limits(&upnp::TRANSPORT).ok_or(OutOfTime("Play"))?;
    let mut result = waiting_on(progress, offer, || upnp::play_within(avt, &limits));
    if result.as_ref().err().and_then(upnp::fault_code) == Some(701) && budget.left() >= MIN_LEFT_FOR_RETRY {
        std::thread::sleep(PLAY_RETRY_WAIT);
        if let Some(limits) = budget.limits(&upnp::TRANSPORT) {
            result = waiting_on(progress, offer, || upnp::play_within(avt, &limits));
        }
    }
    result
}

/// Whether `e` is an action the speaker didn't answer in time.
fn no_answer_in_time(e: &anyhow::Error) -> bool {
    e.downcast_ref::<upnp::ActionError>()
        .is_some_and(|a| matches!(a.failure, upnp::Failure::NoAnswer(crate::device_http::HttpError::NoAnswer(_))))
}

/// End every event subscription of `session`.
fn unsubscribe_all(session: &RendererSession) {
    session.gena.unsubscribe();
    for t in [&session.transport, &session.topology].into_iter().flatten() {
        t.unsubscribe();
    }
}

pub fn stop_session(session: &RendererSession) {
    unsubscribe_all(session);
    if let Err(e) = upnp::stop(&session.renderer.av_transport_control_url) {
        debug!("UPnP Stop on {} failed: {}", session.renderer.friendly_name, e);
    }
}

// -----------------------------------------------------------------------------
// IP / bind helpers (moved from main.rs to keep main.rs lean)
// -----------------------------------------------------------------------------

/// How a speaker is reached, where `--player` cares: a Sonos is listed
/// twice, as UPnP and as AirPlay, and its UPnP entry is the one to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlayerKind {
    Upnp,
    AirPlay,
    Other,
}

/// A listed speaker as `--player` sees it.
#[derive(Clone, Debug)]
pub(crate) struct PlayerCandidate {
    pub id: String,
    /// As the list shows it (a speaker added by address carries its port).
    pub address: String,
    /// The names the list shows for it, and a Sonos speaker's room.
    pub names: Vec<String>,
    /// The other rooms of the Sonos group it plays to.
    pub group_rooms: Vec<String>,
    /// The device's own description name (a UPnP friendlyName with its
    /// model), where the list shows another.
    pub device_name: Option<String>,
    pub kind: PlayerKind,
    /// Added by address: it needs no search to be found.
    pub manual: bool,
    pub hidden: bool,
    /// The settings record it being used before: the last speaker, its own
    /// settings, a pairing or password, or added by address.
    pub used_before: bool,
}

/// How `--player`'s text matched the speaker taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerMatchKind {
    Address,
    FullName,
    PartOfName,
    /// A room inside a Sonos group.
    GroupRoom,
    /// Part of the device's own description name.
    DeviceName,
}

impl PlayerMatchKind {
    pub fn describe(self) -> &'static str {
        match self {
            PlayerMatchKind::Address => "its address",
            PlayerMatchKind::FullName => "its full name",
            PlayerMatchKind::PartOfName => "part of its name",
            PlayerMatchKind::GroupRoom => "a room in its Sonos group",
            PlayerMatchKind::DeviceName => "part of its device name",
        }
    }
}

/// The speaker `--player` takes, how it matched, and how many others
/// matched the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerMatch {
    pub id: String,
    pub how: PlayerMatchKind,
    pub also_fitted: usize,
}

impl PlayerMatch {
    /// The log line that says which speaker (`name`) `query` took, when
    /// that wasn't plain: a partial match, or one of several. So a wrong
    /// pick can be understood from the log.
    pub fn explain(&self, query: &str, name: &str) -> Option<String> {
        let plain = matches!(self.how, PlayerMatchKind::Address | PlayerMatchKind::FullName);
        if plain && self.also_fitted == 0 {
            return None;
        }
        Some(format!(
            "--player {:?}: taking {:?}, matched by {}; {} other speaker(s) matched the same way",
            query,
            name,
            self.how.describe(),
            self.also_fitted
        ))
    }
}

/// The address in `text` (an IP, with or without a port or brackets), if
/// it is one.
fn parse_address(text: &str) -> Option<(IpAddr, Option<u16>)> {
    if let Ok(sa) = text.parse::<SocketAddr>() {
        return Some((sa.ip(), Some(sa.port())));
    }
    text.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok().map(|ip| (ip, None))
}

/// Which listed speaker (`rows`, in list order) `query` names. An address
/// (with or without a port) matches addresses only. Otherwise the first
/// step with any match decides: the full name (any letter case,
/// surrounding spaces ignored); else part of a name, among the speakers
/// shown (a part is a search, and a search finds only what the list shows;
/// hidden ones are reached by full name or address); else part of a room
/// inside a Sonos group; else part of the device's own description name,
/// both also among those shown. Among several matches: shown before
/// hidden, a speaker's UPnP entry before its AirPlay one, then ones used
/// before, then list order. Nothing is refused for being ambiguous.
pub(crate) fn rank_player_match(query: &str, rows: &[PlayerCandidate]) -> Option<PlayerMatch> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let lower = q.to_lowercase();
    let contains = |names: &[String]| names.iter().any(|n| n.to_lowercase().contains(&lower));
    let steps: Vec<(PlayerMatchKind, Box<dyn Fn(&PlayerCandidate) -> bool + '_>)> = match parse_address(q) {
        Some((ip, port)) => vec![(
            PlayerMatchKind::Address,
            Box::new(move |c: &PlayerCandidate| match parse_address(&c.address) {
                Some((at, at_port)) => at == ip && (port.is_none() || at_port.is_none() || port == at_port),
                // A speaker added by a host name: its listed text.
                None => c.address == q || c.address.strip_prefix(q).is_some_and(|rest| rest.starts_with(':')),
            }),
        )],
        None => vec![
            (
                PlayerMatchKind::FullName,
                Box::new(|c: &PlayerCandidate| c.names.iter().any(|n| n.trim().to_lowercase() == lower)),
            ),
            (PlayerMatchKind::PartOfName, Box::new(|c: &PlayerCandidate| !c.hidden && contains(&c.names))),
            (PlayerMatchKind::GroupRoom, Box::new(|c: &PlayerCandidate| !c.hidden && contains(&c.group_rooms))),
            (
                PlayerMatchKind::DeviceName,
                Box::new(|c: &PlayerCandidate| !c.hidden && c.device_name.iter().any(|n| n.to_lowercase().contains(&lower))),
            ),
        ],
    };
    steps.iter().find_map(|(how, fits)| {
        let found: Vec<(usize, &PlayerCandidate)> = rows.iter().enumerate().filter(|(_, c)| fits(c)).collect();
        let upnp_at: Vec<IpAddr> = found
            .iter()
            .filter(|(_, c)| c.kind == PlayerKind::Upnp)
            .filter_map(|(_, c)| parse_address(&c.address).map(|(ip, _)| ip))
            .collect();
        // An AirPlay entry whose speaker also has a UPnP entry among these.
        let twin = |c: &PlayerCandidate| {
            c.kind == PlayerKind::AirPlay
                && parse_address(&c.address).is_some_and(|(ip, _)| upnp_at.contains(&ip))
        };
        let best = found
            .iter()
            .min_by_key(|(i, c)| (c.hidden, twin(c), !c.used_before, *i))?;
        Some(PlayerMatch { id: best.1.id.clone(), how: *how, also_fitted: found.len() - 1 })
    })
}

fn player_wait_ended(query: &str) -> String {
    format!("--player {query:?}: where the sound plays changed meanwhile; no longer waiting")
}

/// How long `--player` waits for discovery's first look before taking a
/// match from what is listed (as long as the start-up wait for the first
/// UPnP sweep was).
const PLAYER_FIRST_LOOK: Duration = Duration::from_secs(5);

/// The `--player` wait's decisions, apart from its thread and its clock.
pub(crate) struct WishSteps {
    started: Instant,
    /// The speaker tried during its current appearance in the list.
    pub tried: Option<String>,
}

impl WishSteps {
    pub fn new(started: Instant) -> Self {
        Self { started, tried: None }
    }

    /// The speaker to connect to now, given the listed `rows`; `searched`:
    /// the first UPnP sweep has been published (or there is no UPnP
    /// discovery); `busy`: a connect is under way. Nothing is chosen from
    /// discovery's results before its first look is done: a Sonos's AirPlay
    /// entry, or a near match, usually answers before the UPnP entry of the
    /// speaker meant. A speaker added by address needs no search.
    pub fn next(
        &mut self,
        now: Instant,
        query: &str,
        rows: &[PlayerCandidate],
        searched: bool,
        busy: bool,
    ) -> Option<PlayerMatch> {
        let looked = searched || now >= self.started + PLAYER_FIRST_LOOK;
        let found = if looked {
            rank_player_match(query, rows)
        } else {
            let manual: Vec<PlayerCandidate> = rows.iter().filter(|c| c.manual).cloned().collect();
            rank_player_match(query, &manual)
        };
        match found {
            None => {
                if looked {
                    self.tried = None;
                }
                None
            }
            Some(m) if busy || self.tried.as_deref() == Some(m.id.as_str()) => None,
            Some(m) => {
                self.tried = Some(m.id.clone());
                Some(m)
            }
        }
    }
}

/// Where a `--player` wait stands, for the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerWish {
    /// Looking for a speaker `query` names.
    Looking { query: String },
    /// Found `speaker` and connecting to it failed, for `reason` (as the
    /// connect told it, if it did); it is tried again when it comes back
    /// to the list. `needs_person`: it can't work without someone at the
    /// speaker (PIN pairing), and the reason says what to do.
    Failed { query: String, speaker: String, reason: Option<String>, needs_person: bool },
}

impl PlayerWish {
    fn failed(query: &str, speaker: String, reason: Option<String>) -> Self {
        let needs_person = reason.as_deref().is_some_and(needs_the_person);
        PlayerWish::Failed { query: query.to_string(), speaker, reason, needs_person }
    }
}

/// Whether a connect's failure is one only the person can get past: the
/// speaker wants PIN pairing, which a connect nobody clicked can't start
/// (see [`pin_pairing_message`]); trying again would only fail again.
pub(crate) fn needs_the_person(reason: &str) -> bool {
    reason.contains("click it in the speaker list") || reason.ends_with(" — follow the prompt.")
}

/// How often the window tries a failed settings save again.
pub const SETTINGS_RETRY: Duration = Duration::from_secs(5);

/// The words a failed background connect's fault begins with.
const CONNECT_FAILED_PREFIX: &str = "Couldn't connect to speaker: ";

/// The address that names this PC to other devices, the first of: the one
/// given with `--advertise-ip`; a specific `--bind` address (the server
/// answers there only); the address of the adapter chosen for discovery;
/// the one Windows uses toward the speaker being played on; the default
/// route's; any local IPv4; else loopback, while the PC has no network
/// (a link then still opens on this PC). Each lookup runs only if the ones
/// before it found nothing.
pub fn this_pc_address_from(
    explicit: Option<IpAddr>,
    bind: IpAddr,
    adapter: Option<Ipv4Addr>,
    toward_speaker: impl FnOnce() -> Option<IpAddr>,
    default_route: impl FnOnce() -> Option<IpAddr>,
    any_local: impl FnOnce() -> Option<Ipv4Addr>,
) -> IpAddr {
    explicit
        .or_else(|| (!bind.is_unspecified()).then_some(bind))
        .or_else(|| adapter.map(IpAddr::V4))
        .or_else(toward_speaker)
        .or_else(default_route)
        .or_else(|| any_local().map(IpAddr::V4))
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// This PC's address on its default route right now, if it has one.
pub fn default_route_ip() -> Option<IpAddr> {
    route_source_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)))
}

/// The first usable IPv4 address on any adapter (loopback and link-local
/// left out), if there is one.
pub fn first_local_ipv4() -> Option<Ipv4Addr> {
    crate::ssdp::local_ipv4_interfaces().first().copied()
}

/// The source address the OS routing table picks for packets to `peer`
/// (a connected UDP socket sends nothing). None if there is no route or
/// the answer is unusable.
pub fn route_source_ip(peer: IpAddr) -> Option<IpAddr> {
    use std::net::UdpSocket;
    let any: SocketAddr = if peer.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let s = UdpSocket::bind(any).ok()?;
    s.connect((peer, 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// The stream server's base URL (a stream profile's path goes after it)
/// and the `/gena` callback URL, on `ip`.
fn local_urls(ip: IpAddr, port: u16) -> (String, String) {
    let host = match ip {
        IpAddr::V6(v6) => format!("[{}]", v6),
        IpAddr::V4(v4) => v4.to_string(),
    };
    (
        format!("http://{}:{}", host, port),
        format!("http://{}:{}/gena", host, port),
    )
}

/// The one interface SSDP should search from, if the user pinned one
/// (`--advertise-ip`, or `--bind` to a specific address). None = search
/// from every interface.
pub fn pick_ssdp_iface(advertise_ip: Option<IpAddr>, bind: &str) -> Option<std::net::Ipv4Addr> {
    if let Some(IpAddr::V4(v4)) = advertise_ip {
        return Some(v4);
    }
    if let Ok(IpAddr::V4(v4)) = bind.parse::<IpAddr>() {
        if !v4.is_unspecified() {
            return Some(v4);
        }
    }
    None
}

#[allow(dead_code)]
const _: u32 = DEFAULT_QUIESCENT_AFTER_PACKETS;

/// Seconds since the Unix epoch, saturating to 0 if the system clock is
/// before 1970 (which would only happen on a badly misconfigured box).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn finishing_pairing_without_a_waiting_request_respects_later_picks() {
        use crate::model::{Command, SpeakerId, Surface};
        let app = App::for_tests();
        let (sent, commands) = crossbeam_channel::unbounded();
        app.set_command_sink(Box::new(move |from, cmd| {
            sent.send((from, cmd)).unwrap();
            crate::model::command::ticket().1
        }));
        let picks_seen = app.user_pick_count();
        app.play_after_pairing(KITCHEN, picks_seen);
        assert_eq!(commands.try_recv().unwrap(), (Surface::Panel, Command::PlayAfterPairing {
            speaker: SpeakerId::new(KITCHEN), picks_seen,
        }));
        app.note_user_pick();
        app.play_after_pairing(KITCHEN, picks_seen);
        assert!(commands.is_empty());
        assert!(!app.is_speaker_bound());
    }

    #[test]
    fn an_airplay_chime_retries_after_pairing_in_the_window() {
        for (window, close) in [(false, false), (true, true), (true, false)] {
            let mut attempts = 0;
            let mut prompted = false;
            let result = chime_airplay(window, || {
                attempts += 1;
                if attempts == 1 {
                    Err(SelectFailure::NeedsPin { id: "ap".into(), name: "Office".into(), reason: PinReason::Required, details: String::new() })
                } else { Ok(()) }
            }, |id, name| {
                assert_eq!((id.as_str(), name.as_str()), ("ap", "Office"));
                prompted = true;
                if close { Err(CODE_WINDOW_CLOSED.into()) } else { Ok(()) }
            });
            assert_eq!(prompted, window);
            assert_eq!(attempts, if window && !close { 2 } else { 1 });
            assert_eq!(result, if !window { Err("Office needs its code".into()) } else if close { Err(CODE_WINDOW_CLOSED.into()) } else { Ok(()) });
        }
    }

    #[test]
    fn asking_after_a_code_expires_shows_another_code_for_the_same_request() {
        use crate::airplay::discovery::AIRPLAY_SERVICE;
        use crate::airplay::fake_raop::{Answer, FakeRaop, Reply};
        use crate::settings::{Act, SettingsPort, port::{CodeStage, Wanted}, real::AppSettings};
        let failed_show = AtomicBool::new(true);
        let receiver = FakeRaop::start(move |req| {
            if req.uri == "/pair-pin-start" && failed_show.swap(false, Ordering::SeqCst) {
                Answer::Reply(Reply::status(503, "Unavailable"))
            } else { Answer::Reply(Reply::ok()) }
        });
        let lan = crate::discovery_net::LanTargets::from_networks([("192.168.1.10".parse().unwrap(), 24)]);
        let (app, discovery) = airplay_test_app_with(Arc::new(AirPlayDiscoveryState::with_lan(lan.with_fakes_on_loopback())));
        let txt = &[("deviceid", "B8:E9:37:35:AC:11"), ("features", "0x445F8A00,0x1C340")][..];
        discovery.ingest_for_tests(AIRPLAY_SERVICE, &mdns_sd::ServiceInfo::new(
            AIRPLAY_SERVICE, "Kitchen", "k.local.", "127.0.0.1", receiver.port(), txt).unwrap());
        let port = AppSettings::new(app.clone());
        let at = |stage| matches!(port.wanted_dialog(), Some(Wanted::PairingCode(a)) if a.stage == stage);
        let (sent, commands) = crossbeam_channel::unbounded();
        app.set_command_sink(Box::new(move |_, cmd| {
            sent.send(cmd).unwrap();
            crate::model::command::ticket().1
        }));
        let (paired, request) = std::sync::mpsc::channel();
        let job = Arc::new(crate::engine::cancel::Binding::new(crate::model::SpeakerId::new(KITCHEN), Some(40), app.user_pick_count(), || {}));
        let scope = job.enter();
        app.begin_pin_pairing(KITCHEN.into(), "Kitchen".into(), Some(paired));
        drop(scope);
        assert!(eventually(|| at(CodeStage::NoAnswer)));
        // A second click while the TV is asked again changes nothing.
        port.act(Act::NewCode(KITCHEN.into()));
        port.act(Act::NewCode(KITCHEN.into()));
        assert_eq!(app.current_error(), None);
        assert!(eventually(|| at(CodeStage::Showing)));
        {
            let mut slot = app.pin_ceremony.lock().unwrap();
            let PinStage::AwaitingPin { expires, .. } = &mut slot.as_mut().unwrap().stage else { panic!("expected the code") };
            *expires = Instant::now();
        }
        assert!(at(CodeStage::TimedOut));
        port.act(Act::NewCode(KITCHEN.into()));
        assert!(eventually(|| at(CodeStage::Showing)));
        // A slow test machine can take the second click as a third ask.
        assert!(receiver.requests().iter().filter(|r| r.uri == "/pair-pin-start").count() >= 3);
        assert!(matches!(request.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        port.act(Act::CancelPairing(KITCHEN.into()));
        assert!(eventually(|| app.pin_pairing_device().is_none()));
        port.act(Act::NewCode(KITCHEN.into()));
        assert_eq!(app.current_error().as_deref(), Some("That pairing had already ended. Pick the speaker again to pair it."));
        assert!(commands.is_empty());
    }

    #[test]
    fn picking_another_tv_ends_the_old_ceremony_and_shows_the_new_code() {
        use crate::airplay::discovery::AIRPLAY_SERVICE;
        use crate::airplay::fake_raop::{Answer, FakeRaop, Reply};
        let (showing, asked) = crossbeam_channel::bounded(1);
        let (finish, resume) = crossbeam_channel::bounded(1);
        let kitchen = FakeRaop::start(move |req| {
            if req.uri == "/pair-pin-start" {
                showing.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            Answer::Reply(Reply::ok())
        });
        let den_tv = FakeRaop::start(|_| Answer::Reply(Reply::ok()));
        let lan = crate::discovery_net::LanTargets::from_networks([("192.168.1.10".parse().unwrap(), 24)]);
        let (app, discovery) = airplay_test_app_with(Arc::new(AirPlayDiscoveryState::with_lan(lan.with_fakes_on_loopback())));
        for (name, device, tv) in [("Kitchen", "B8:E9:37:35:AC:11", &kitchen), ("Den", "B8:E9:37:35:AC:12", &den_tv)] {
            let txt = &[("deviceid", device), ("features", "0x445F8A00,0x1C340")][..];
            discovery.ingest_for_tests(AIRPLAY_SERVICE, &mdns_sd::ServiceInfo::new(
                AIRPLAY_SERVICE, name, &format!("{name}.local."), "127.0.0.1", tv.port(), txt).unwrap());
        }
        let (paired, old_ended) = std::sync::mpsc::channel();
        app.begin_pin_pairing(KITCHEN.into(), "Kitchen".into(), Some(paired));
        asked.recv_timeout(Duration::from_secs(5)).unwrap();
        let old = app.pin_ceremony.lock().unwrap().as_ref().unwrap().cancelled.clone();
        let den = "airplay:B8E93735AC12";
        app.begin_pin_pairing(den.into(), "Den".into(), None);
        assert!(old.load(Ordering::Acquire));
        assert!(eventually(|| app.pending_pin_pairing().is_some_and(|(id, _)| id == den)));
        finish.send(()).unwrap();
        assert_eq!(old_ended.recv_timeout(Duration::from_secs(5)), Err(std::sync::mpsc::RecvTimeoutError::Disconnected));
        // The first TV finally answered. The second TV still owns the prompt and its answers.
        assert!(app.ask_for_new_code(den));
        assert!(eventually(|| den_tv.requests().iter().filter(|q| q.uri == "/pair-pin-start").count() == 2));
        assert!(eventually(|| app.pending_pin_pairing().is_some_and(|(id, _)| id == den)));
        app.submit_pin(den, None);
        assert!(eventually(|| app.pin_pairing_device().is_none()));
    }

    #[test]
    fn a_dialog_for_another_tv_cannot_answer_the_waiting_ceremony() {
        use crate::settings::{Act, SettingsPort, real::AppSettings};
        let app = test_app();
        let (answers, rx) = crossbeam_channel::bounded(1);
        *app.pin_ceremony.lock().unwrap() = Some(PinCeremonyState {
            id: "kitchen".into(), name: "Kitchen".into(), code_try: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            stage: PinStage::AwaitingPin { answers, expires: Instant::now() + Duration::from_secs(90) },
        });
        let port = AppSettings::new(app.clone());
        for act in [Act::NewCode("den".into()), Act::SubmitCode { id: "den".into(), code: "1234".into() }] {
            port.act(act);
            assert_eq!(app.current_error().as_deref(), Some("That pairing had already ended. Pick the speaker again to pair it."));
            app.dismiss_error();
        }
        port.act(Act::CancelPairing("den".into()));
        assert!(app.pairing_code_ask().is_some_and(|a| a.id == "kitchen"));
        assert!(rx.try_recv().is_err());
        port.act(Act::SubmitCode { id: "kitchen".into(), code: "5678".into() });
        assert!(matches!(rx.try_recv(), Ok(PinAnswer::Code(code)) if code == "5678"));
    }

    #[test]
    fn the_chime_keeps_its_position_while_the_session_lock_is_held() {
        let app = test_app();
        bind(&app, upnp_session(""));
        app.play_test_sound();
        let session = app.session.lock().unwrap();
        let mut samples = [0; 128];
        app.mix_test_sound(&mut samples, 2);
        assert_eq!(samples, [0; 128]);
        assert_eq!(*app.test_sound.lock().unwrap(), Some((app.session_number(), 0)));
        drop(session);
        app.mix_test_sound(&mut samples, 2);
        assert!(samples.iter().any(|s| *s != 0));
    }

    #[test]
    fn the_shared_chime_stops_when_its_session_is_replaced_or_released() {
        for replace in [false, true] {
            let app = test_app();
            bind(&app, upnp_session(""));
            app.play_test_sound();
            let mut samples = [0; 128];
            app.mix_test_sound(&mut samples, 2);
            assert!(samples.iter().any(|s| *s != 0));
            if replace {
                bind(&app, upnp_session(""));
            } else {
                app.session.lock().unwrap().take();
            }
            samples.fill(10);
            app.mix_test_sound(&mut samples, 2);
            assert_eq!(samples, [10; 128]);
            assert!(!app.test_sound_playing());
        }
    }

    #[test]
    fn local_urls_bracket_ipv6_hosts() {
        let (s, c) = local_urls("192.168.2.100".parse().unwrap(), 8080);
        assert_eq!(s, "http://192.168.2.100:8080");
        assert_eq!(c, "http://192.168.2.100:8080/gena");
        let (s, _) = local_urls("fe80::1".parse().unwrap(), 8080);
        assert_eq!(format!("{s}{}", StreamProfile::Legacy.path()), "http://[fe80::1]:8080/stream.raw");
    }

    #[test]
    fn route_source_ip_uses_the_route_to_the_peer() {
        assert_eq!(route_source_ip("127.0.0.1".parse().unwrap()), Some("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn ssdp_searches_every_interface_unless_pinned() {
        assert_eq!(pick_ssdp_iface(None, "0.0.0.0"), None);
        assert_eq!(pick_ssdp_iface(Some(ip("192.168.2.100")), "0.0.0.0"), Some("192.168.2.100".parse().unwrap()));
        assert_eq!(pick_ssdp_iface(None, "10.0.0.5"), Some("10.0.0.5".parse().unwrap()));
    }


    /// [`App::for_tests`] with privacy mode on.
    fn test_app() -> Arc<App> {
        let app = App::for_tests();
        app.set_privacy_mode(true);
        app
    }

    /// Run only by [`test_app_never_reads_the_settings_file`], in a child
    /// process whose settings file turns reconnect-on-drop off.
    #[test]
    #[ignore = "run in a child process by test_app_never_reads_the_settings_file"]
    fn test_app_settings_probe() {
        assert!(test_app().user_config.lock().unwrap().auto_reconnect_on_drop, "test_app() used the settings file");
    }

    #[test]
    fn test_app_never_reads_the_settings_file() {
        // The settings folder comes from APPDATA, which a test can't change
        // for itself without racing every other test: run the probe in a
        // process of its own.
        let dir = std::env::temp_dir().join(format!("sts-test-app-settings-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("StreamToSpeaker")).unwrap();
        std::fs::write(
            dir.join("StreamToSpeaker").join("config.json"),
            r#"{"auto_reconnect_on_drop": false}"#,
        )
        .unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "app::tests::test_app_settings_probe", "--test-threads=1"])
            .env("APPDATA", &dir)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(text.contains("1 passed"), "the probe didn't run, or failed:\n{text}");
        assert!(out.status.success(), "{text}");
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn windows_audio_starts_unknown_and_follows_the_packets() {
        let app = test_app();
        // The app starts out reading from Windows before anything came.
        assert_eq!(app.diagnostics_snapshot().windows_audio, WindowsAudio::NothingYet);
        app.note_source_packet(false);
        assert_eq!(app.windows_audio(), WindowsAudio::Silent);
        app.note_source_packet(true);
        assert_eq!(app.windows_audio(), WindowsAudio::Playing);
        // A silent packet right after sound is a pause, not silence yet.
        app.note_source_packet(false);
        assert_eq!(app.windows_audio(), WindowsAudio::Playing);
        app.stream_active.store(false, Ordering::Release);
        assert_eq!(app.windows_audio(), WindowsAudio::Stopped);
    }

    #[test]
    fn streaming_status_needs_packets_from_windows() {
        let app = test_app();
        // At launch the stream flag is set but nothing has come.
        assert!(app.stream_active.load(Ordering::Acquire));
        assert!(!app.receiving_windows_audio());
        // A silent stream still counts, as before.
        app.note_source_packet(false);
        assert!(app.receiving_windows_audio());
        app.note_source_packet(true);
        assert!(app.receiving_windows_audio());
        app.stream_active.store(false, Ordering::Release);
        assert!(!app.receiving_windows_audio());
    }

    /// What the fake UPnP speaker was asked to do.
    #[derive(Debug, PartialEq)]
    enum Soap {
        Volume(u32),
        Mute(bool),
    }

    /// A UPnP speaker on loopback that reports each SetVolume and SetMute
    /// and answers only when the test releases it, like a slow speaker.
    fn slow_soap_speaker() -> (
        String,
        crossbeam_channel::Receiver<Soap>,
        crossbeam_channel::Sender<()>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/RenderingControl/Control", listener.local_addr().unwrap());
        let (got_tx, got_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded::<()>();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !String::from_utf8_lossy(&req).contains("</s:Envelope>") {
                    match conn.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&req).into_owned();
                let field = |name: &str| {
                    req.split(&format!("<{}>", name))
                        .nth(1)
                        .and_then(|r| r.split('<').next())
                        .and_then(|v| v.parse::<u32>().ok())
                };
                let what = match (field("DesiredVolume"), field("DesiredMute")) {
                    (Some(level), _) => Soap::Volume(level),
                    (None, Some(m)) => Soap::Mute(m != 0),
                    (None, None) => return,
                };
                if got_tx.send(what).is_err() || release_rx.recv().is_err() {
                    return;
                }
                let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        (url, got_rx, release_tx)
    }

    /// Bind `session` the way selecting a speaker does.
    fn bind(app: &App, session: ActiveSession) {
        let mut guard = app.session.lock().unwrap();
        app.bind_session(&mut guard, session);
    }

    fn upnp_session(control_url: &str) -> ActiveSession {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        ActiveSession::Upnp(RendererSession {
            renderer: Renderer {
                friendly_name: "Kitchen".into(),
                ip,
                base_url: String::new(),
                av_transport_control_url: String::new(),
                rendering_control_control_url: control_url.into(),
                rendering_control_event_url: String::new(),
                av_transport_event_url: None,
                connection_manager_control_url: None,
                udn: Some("uuid:kitchen".into()),
                zone_group_topology_control_url: None,
                group_rendering_control_control_url: None,
                group_rendering_control_event_url: None,
                zone_name: None,
                group_members: Vec::new(),
                group_member_addrs: Vec::new(),
                host_addrs: vec![ip],
                model_name: None,
                udn_conflict: false,
            },
            gena: GenaManager::new("http://127.0.0.1:1/gena".into()),
            transport: None,
            topology: None,
            stream_uri: String::new(),
            stream_grant: None,
            id_override: None,
            profile: StreamProfile::Legacy,
            stream_given_at: Instant::now(),
            played_at: Instant::now(),
        })
    }

    /// A Sonos whose every service is on `fake`: it is offered the
    /// original stream only, so one offer is all a test of it sees.
    fn sonos_on(fake: &crate::fake_renderer::FakeRenderer) -> Renderer {
        let ActiveSession::Upnp(s) = upnp_session(&fake.url("/RenderingControl/Control")) else { unreachable!() };
        Renderer {
            base_url: fake.url(""),
            av_transport_control_url: fake.url("/AVTransport/Control"),
            rendering_control_event_url: fake.url("/RenderingControl/Event"),
            zone_group_topology_control_url: Some(fake.url("/ZoneGroupTopology/Control")),
            ..s.renderer
        }
    }

    /// Nothing remembered, the app's own budget.
    fn first_offer() -> StreamOffer {
        StreamOffer { base_url: "http://127.0.0.1:1".into(), remembered: None, initial_buffer_ms: 0, budget: CONNECT_BUDGET }
    }

    /// A renderer (not a Sonos) whose every service is on `fake`.
    fn renderer_on(fake: &crate::fake_renderer::FakeRenderer) -> Renderer {
        Renderer { zone_group_topology_control_url: None, ..sonos_on(fake) }
    }

    /// Progress reports a connect made, in order.
    fn steps_of(f: impl FnOnce(&(dyn Fn(ConnectStep) + Sync))) -> Vec<ConnectStep> {
        let seen = Mutex::new(Vec::new());
        f(&|step| seen.lock().unwrap().push(step));
        seen.into_inner().unwrap()
    }

    #[test]
    fn no_further_offer_starts_when_the_budget_is_nearly_spent() {
        use crate::fake_renderer::{FakeRenderer, Reply, AV_TRANSPORT};
        let fake = FakeRenderer::start(|req| match req.soap_action() {
            Some("SetAVTransportURI") => Reply::soap_fault(714, "Illegal MIME-type"),
            Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
            None => Reply::status(404, "Not Found", &[], ""),
        });
        // Less than an offer's worth of budget once the first is refused.
        let offer = StreamOffer { budget: Duration::from_secs(5), ..first_offer() };
        let e = start_session(renderer_on(&fake), &offer, "http://127.0.0.1:1/gena", None, &|_| {}).err().unwrap();
        assert_eq!(e.formats_tried, 1);
        assert_eq!(e.out_of_time, None);
        assert_eq!(fake.soap_actions(), ["Stop", "SetAVTransportURI"]);
    }

    #[test]
    fn a_speaker_that_never_answers_costs_no_more_than_the_budget() {
        use crate::fake_renderer::{FakeRenderer, Reply, AV_TRANSPORT};
        let fake = FakeRenderer::start(|req| match req.soap_action() {
            Some("SetAVTransportURI") => Reply::silent(),
            Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
            None => Reply::status(404, "Not Found", &[], ""),
        });
        let offer = StreamOffer { budget: Duration::from_millis(1500), ..first_offer() };
        let start = Instant::now();
        let e = start_session(renderer_on(&fake), &offer, "http://127.0.0.1:1/gena", None, &|_| {}).err().unwrap();
        assert!(start.elapsed() < Duration::from_secs(15), "{:?}", start.elapsed());
        assert_eq!(e.out_of_time, Some(Duration::from_millis(1500)), "{:#}", e.error);
        let app = test_app();
        assert_eq!(
            app.upnp_failure_message("Kitchen", "127.0.0.1".parse().unwrap(), &e),
            "Kitchen was too slow to answer, so Stream To Speaker stopped trying after 1.5 seconds. Try again in a \
             moment; if it keeps happening, restart Kitchen."
        );
    }

    #[test]
    fn a_slow_answer_and_another_offer_are_reported_as_they_happen() {
        use crate::fake_renderer::{FakeRenderer, Reply, AV_TRANSPORT};
        let sets = Mutex::new(0);
        let fake = FakeRenderer::start(move |req| match req.soap_action() {
            Some("SetAVTransportURI") => {
                let mut n = sets.lock().unwrap();
                *n += 1;
                match *n {
                    1 => Reply::soap_fault(714, "Illegal MIME-type"),
                    // The second offer: answered late.
                    _ => Reply::soap_ok(AV_TRANSPORT, "SetAVTransportURI", &[]).after(Duration::from_millis(3500)),
                }
            }
            Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
            None if req.method == "GET" => Reply::status(404, "Not Found", &[], ""),
            None => Reply::raw("HTTP/1.1 200 OK\r\nSID: uuid:steps\r\nTIMEOUT: Second-1800\r\nContent-Length: 0\r\n\r\n"),
        });
        let steps = steps_of(|progress| {
            let started = start_session(renderer_on(&fake), &first_offer(), "http://127.0.0.1:1/gena", None, progress).unwrap();
            started.session.gena.unsubscribe();
        });
        crate::gena::registry().remove("uuid:steps");
        assert_eq!(
            steps,
            [
                ConnectStep { offer: 1, waiting: false },
                ConnectStep { offer: 1, waiting: true },
                ConnectStep { offer: 1, waiting: false },
            ]
        );
    }

    #[test]
    fn progress_reads_as_what_is_happening() {
        let text = |offer, waiting| ConnectStep { offer, waiting }.text();
        assert_eq!(text(0, false), None);
        assert_eq!(text(0, true).as_deref(), Some("Waiting for the speaker to answer…"));
        assert_eq!(text(1, false).as_deref(), Some("The speaker didn't take the stream. Offering it another way…"));
        assert_eq!(text(1, true).as_deref(), Some("Offering the stream another way. Waiting for the speaker to answer…"));
        assert_eq!(text(2, false).as_deref(), Some("The speaker didn't take that either. Offering the stream a third way…"));
        assert_eq!(text(2, true).as_deref(), Some("Offering the stream a third way. Waiting for the speaker to answer…"));
    }

    /// A renderer that answers each AVTransport action with `play`'s
    /// answers to Play in turn (then success) and accepts the rest.
    fn renderer_answering_play(play: Vec<crate::fake_renderer::Reply>) -> crate::fake_renderer::FakeRenderer {
        use crate::fake_renderer::{FakeRenderer, Reply, AV_TRANSPORT};
        let play = Mutex::new(play.into_iter());
        FakeRenderer::start(move |req| match req.soap_action() {
            Some("Play") => play.lock().unwrap().next().unwrap_or_else(|| Reply::soap_ok(AV_TRANSPORT, "Play", &[])),
            Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
            // GENA SUBSCRIBE / UNSUBSCRIBE.
            None => Reply::raw("HTTP/1.1 200 OK\r\nSID: uuid:fake-session\r\nTIMEOUT: Second-1800\r\nContent-Length: 0\r\n\r\n"),
        })
    }

    #[test]
    fn a_play_refused_after_the_uri_was_taken_is_followed_by_stop() {
        use crate::fake_renderer::Reply;
        let fake = renderer_answering_play(vec![Reply::soap_fault(704, "Playing failed")]);
        let r = start_session(sonos_on(&fake), &first_offer(), "http://127.0.0.1:1/gena", None, &|_| {});
        assert!(r.is_err());
        // The renderer holds our URI; it is told to let go of it.
        assert_eq!(fake.soap_actions(), ["Stop", "SetAVTransportURI", "Play", "Stop"]);
    }

    #[test]
    fn play_answered_transition_not_available_is_tried_once_more() {
        use crate::fake_renderer::Reply;
        // 701: the renderer is still busy taking the URI.
        let fake = renderer_answering_play(vec![Reply::soap_fault(701, "Transition not available")]);
        let session = start_session(sonos_on(&fake), &first_offer(), "http://127.0.0.1:1/gena", None, &|_| {})
            .expect("the second Play is accepted")
            .session;
        assert_eq!(fake.soap_actions(), ["Stop", "SetAVTransportURI", "Play", "Play"]);
        session.gena.unsubscribe();
        crate::gena::registry().remove("uuid:fake-session");

        // Refused twice: given up, and the renderer is stopped.
        let fake = renderer_answering_play(vec![
            Reply::soap_fault(701, "Transition not available"),
            Reply::soap_fault(701, "Transition not available"),
        ]);
        assert!(start_session(sonos_on(&fake), &first_offer(), "http://127.0.0.1:1/gena", None, &|_| {}).is_err());
        assert_eq!(fake.soap_actions(), ["Stop", "SetAVTransportURI", "Play", "Play", "Stop"]);
    }

    /// A speaker added by the address of `fake`'s description.
    fn manual_upnp(fake: &crate::fake_renderer::FakeRenderer) -> ManualSpeaker {
        ManualSpeaker { kind: ManualKind::Upnp, name: String::new(), host: fake.url("/description.xml"), port: 0 }
    }

    #[test]
    fn a_refused_connect_tells_the_person_why_in_the_apps_words() {
        use crate::fake_renderer::{renderer_description, FakeRenderer, Reply, AV_TRANSPORT};
        let fake = FakeRenderer::start(|req| match req.soap_action() {
            Some("SetAVTransportURI") => Reply::soap_fault(714, "Illegal MIME-type"),
            Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
            None => Reply::ok(&renderer_description("Den Amp", &[])),
        });
        let app = serving_app();
        let Err(msg) = app.start_upnp_manual(&manual_upnp(&fake)) else { panic!("connected") };
        // It lists no types, so it is offered three in turn.
        assert_eq!(
            msg,
            "Den Amp didn't take the stream in any of the 3 ways Stream To Speaker offered it. \
             (Den Amp answered: UPnP error 714, \"Illegal MIME-type\".)"
        );
    }

    // ---- stream profiles, end to end ----------------------------------

    /// An app whose HTTP server is up on loopback, so a renderer can fetch
    /// what it is given, and whose settings stay in memory.
    fn serving_app() -> Arc<App> {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let app = App::new(
            AppConfig {
                advertise_ip: None,
                bind: SocketAddr::from(([127, 0, 0, 1], port)),
                initial_buffer_ms: 0,
                silence_packets_threshold: 0,
                no_silence_injection: false,
                no_discovery: true,
                web_enabled: false,
                ssdp_iface: None,
            },
            None,
            None,
        );
        *app.user_config.lock().unwrap() = UserConfig::in_memory();
        crate::http_server::start_http_server(crate::http_server::HttpServerConfig::minimal(app.config.bind, app.hub.clone()))
            .unwrap();
        app
    }

    /// What a renderer that checks the stream saw of it.
    #[derive(Default, Debug)]
    struct Probed {
        /// Each URI it was given, the type its metadata named, and the
        /// type a HEAD of it answered with.
        offered: Vec<(String, String, String)>,
        /// The type each GET of the stream it plays answered with.
        fetched: Vec<String>,
    }

    /// The `Content-Type` of `method uri`, from our own server.
    fn stream_type(method: &str, uri: &str) -> String {
        use std::io::{Read, Write};
        let url = url::Url::parse(uri).unwrap();
        let mut s = std::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let host = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
        s.write_all(
            format!("{method} {} HTTP/1.1\r\nHost: {host}\r\ngetContentFeatures.dlna.org: 1\r\nConnection: close\r\n\r\n", url.path())
                .as_bytes(),
        )
        .unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && s.read(&mut b).unwrap_or(0) == 1 {
            head.push(b[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        head.split("\r\n").find_map(|l| l.strip_prefix("Content-Type: ")).unwrap_or("").to_string()
    }

    /// A renderer like the KEF LSX II LT: its ConnectionManager lists
    /// `sinks` (None: it has none), it takes a URI only when a HEAD of it
    /// answers with a type in `plays`, and on Play it fetches the stream.
    /// `more`: further services in its description.
    fn strict_renderer(
        sinks: Option<&'static str>,
        plays: &'static [&'static str],
        more: &'static [&'static str],
    ) -> (crate::fake_renderer::FakeRenderer, Arc<Mutex<Probed>>) {
        use crate::fake_renderer::{renderer_description, FakeRenderer, Reply, AV_TRANSPORT, CONNECTION_MANAGER};
        let probed = Arc::new(Mutex::new(Probed::default()));
        let seen = probed.clone();
        let current = Mutex::new(String::new());
        let fake = FakeRenderer::start(move |req| match req.soap_action() {
            Some("GetProtocolInfo") => match sinks {
                Some(list) => Reply::soap_ok(CONNECTION_MANAGER, "GetProtocolInfo", &[("Source", ""), ("Sink", list)]),
                None => Reply::status(404, "Not Found", &[], ""),
            },
            Some("SetAVTransportURI") => {
                let uri = req.soap_arg("CurrentURI").unwrap();
                let didl = req.soap_arg("CurrentURIMetaData").unwrap();
                let doc = roxmltree::Document::parse(&didl).unwrap();
                let info = doc.descendants().find(|n| n.tag_name().name() == "res").unwrap().attribute("protocolInfo").unwrap();
                let didl_type = info.split(':').nth(2).unwrap().to_string();
                let head_type = stream_type("HEAD", &uri);
                seen.lock().unwrap().offered.push((uri.clone(), didl_type, head_type.clone()));
                if plays.iter().any(|t| t.eq_ignore_ascii_case(&head_type)) {
                    *current.lock().unwrap() = uri;
                    Reply::soap_ok(AV_TRANSPORT, "SetAVTransportURI", &[])
                } else {
                    Reply::soap_fault(714, "Illegal MIME-type")
                }
            }
            Some("Play") => {
                let uri = current.lock().unwrap().clone();
                seen.lock().unwrap().fetched.push(stream_type("GET", &uri));
                Reply::soap_ok(AV_TRANSPORT, "Play", &[])
            }
            Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
            None if req.method == "GET" => Reply::ok(&renderer_description("Den Amp", more)),
            // GENA SUBSCRIBE / UNSUBSCRIBE.
            None => Reply::raw("HTTP/1.1 200 OK\r\nSID: uuid:strict\r\nTIMEOUT: Second-1800\r\nContent-Length: 0\r\n\r\n"),
        });
        (fake, probed)
    }

    /// The stream paths a renderer was offered, in order.
    fn offered_paths(probed: &Mutex<Probed>) -> Vec<String> {
        probed.lock().unwrap().offered.iter().map(|(uri, _, _)| url::Url::parse(uri).unwrap().path().to_string()).collect()
    }

    fn end((session, _device): (ActiveSession, DeviceUse)) {
        session.stop();
        crate::gena::registry().remove("uuid:strict");
    }

    #[test]
    fn a_renderer_that_checks_the_type_gets_one_it_lists_on_head_get_and_metadata_alike() {
        // As a KEF LSX II LT on firmware 2.2.91 does.
        let (fake, probed) = strict_renderer(
            Some("http-get:*:audio/wav:*,http-get:*:audio/wave:*,http-get:*:audio/x-wav:*,http-get:*:audio/mpeg:*"),
            &["audio/wav", "audio/wave", "audio/x-wav"],
            &["ConnectionManager"],
        );
        let app = serving_app();
        let m = manual_upnp(&fake);
        let session = app.start_upnp_manual(&m).expect("the speaker plays");
        let p = probed.lock().unwrap();
        // The original stream first, refused at once; then a type it lists.
        let paths: Vec<String> = p.offered.iter().map(|(uri, _, _)| url::Url::parse(uri).unwrap().path().to_string()).collect();
        assert_eq!(paths, ["/stream.raw", "/wav/stream.wav"]);
        let (uri, didl_type, head_type) = &p.offered[1];
        assert!(uri.ends_with("/wav/stream.wav"), "{uri}");
        assert_eq!((didl_type.as_str(), head_type.as_str()), ("audio/wav", "audio/wav"));
        assert_eq!(p.fetched, ["audio/wav"]);
        drop(p);
        // Its list was asked for only after the refusal.
        let actions: Vec<String> = fake.soap_actions();
        assert_eq!(actions, ["Stop", "SetAVTransportURI", "GetProtocolInfo", "Stop", "SetAVTransportURI", "Play"]);
        let ActiveSession::Upnp(s) = &session.0 else { unreachable!() };
        assert_eq!(s.profile, StreamProfile::Wav);
        end(session);
        // Remembered: the next connect plays at the first offer, unasked.
        probed.lock().unwrap().offered.clear();
        let before = fake.soap_actions().len();
        let session = app.start_upnp_manual(&m).unwrap();
        assert_eq!(offered_paths(&probed), ["/wav/stream.wav"]);
        assert_eq!(fake.soap_actions()[before..], ["Stop", "SetAVTransportURI", "Play"]);
        end(session);
    }

    #[test]
    fn a_renderer_that_takes_the_original_stream_gets_exactly_the_requests_it_always_got() {
        // It has a ConnectionManager, and is never asked anything of it.
        let (fake, probed) = strict_renderer(
            Some("http-get:*:audio/wav:*"),
            &["audio/vnd.wave;codec=1", "audio/wav"],
            &["ConnectionManager"],
        );
        let app = serving_app();
        let session = app.start_upnp_manual(&manual_upnp(&fake)).expect("the speaker plays");
        let seen: Vec<(String, String, Option<String>)> = fake
            .requests()
            .iter()
            .map(|r| (r.method.clone(), r.target.clone(), r.soap_action().map(str::to_string)))
            .collect();
        let req = |m: &str, t: &str, a: Option<&str>| (m.to_string(), t.to_string(), a.map(str::to_string));
        assert_eq!(
            seen,
            [
                req("GET", "/description.xml", None),
                req("POST", "/AVTransport/Control", Some("Stop")),
                req("POST", "/AVTransport/Control", Some("SetAVTransportURI")),
                req("POST", "/AVTransport/Control", Some("Play")),
                req("SUBSCRIBE", "/RenderingControl/Event", None),
                // Its transport's events: whether someone else takes it,
                // and when it refills its buffer.
                req("SUBSCRIBE", "/AVTransport/Event", None),
            ]
        );
        let p = probed.lock().unwrap();
        let (uri, didl_type, head_type) = &p.offered[0];
        assert!(uri.ends_with("/stream.raw"), "{uri}");
        assert_eq!((didl_type.as_str(), head_type.as_str()), ("audio/wav", "audio/vnd.wave;codec=1"));
        drop(p);
        let ActiveSession::Upnp(s) = &session.0 else { unreachable!() };
        assert_eq!(s.profile, StreamProfile::Legacy);
        end(session);
    }

    #[test]
    fn without_a_list_the_original_stream_comes_first_and_the_next_follows_a_refusal() {
        let (fake, probed) = strict_renderer(None, &["audio/L16;rate=44100;channels=2"], &[]);
        let app = serving_app();
        let session = app.start_upnp_manual(&manual_upnp(&fake)).expect("the third profile plays");
        assert_eq!(offered_paths(&probed), ["/stream.raw", "/wav/stream.wav", "/l16/stream.raw"]);
        assert_eq!(probed.lock().unwrap().fetched, ["audio/L16;rate=44100;channels=2"]);
        // Each attempt a clean Stop, SetAVTransportURI, Play.
        let actions: Vec<String> = fake.soap_actions().into_iter().filter(|a| a != "GetProtocolInfo").collect();
        assert_eq!(actions, ["Stop", "SetAVTransportURI", "Stop", "SetAVTransportURI", "Stop", "SetAVTransportURI", "Play"]);
        end(session);
    }

    #[test]
    fn a_renderer_refusing_everything_costs_three_attempts() {
        let (fake, probed) = strict_renderer(None, &["audio/flac"], &[]);
        let app = serving_app();
        let Err(msg) = app.start_upnp_manual(&manual_upnp(&fake)) else { panic!("connected") };
        assert_eq!(offered_paths(&probed).len(), 3);
        assert_eq!(
            msg,
            "Den Amp didn't take the stream in any of the 3 ways Stream To Speaker offered it. \
             (Den Amp answered: UPnP error 714, \"Illegal MIME-type\".)"
        );
    }

    #[test]
    fn a_sonos_gets_the_original_stream_and_nothing_else() {
        let (fake, probed) = strict_renderer(Some("http-get:*:audio/wav:*"), &[], &["ZoneGroupTopology", "ConnectionManager"]);
        let app = serving_app();
        assert!(app.start_upnp_manual(&manual_upnp(&fake)).is_err());
        assert_eq!(offered_paths(&probed), ["/stream.raw"]);
        // Never asked for its list.
        assert!(!fake.soap_actions().iter().any(|a| a == "GetProtocolInfo"), "{:?}", fake.soap_actions());
        let p = probed.lock().unwrap();
        assert_eq!((p.offered[0].1.as_str(), p.offered[0].2.as_str()), ("audio/wav", "audio/vnd.wave;codec=1"));
    }

    #[test]
    fn the_profile_a_speaker_played_is_where_it_starts_next_time() {
        let (fake, probed) = strict_renderer(None, &["audio/wav"], &[]);
        let app = serving_app();
        let m = manual_upnp(&fake);
        let session = app.start_upnp_manual(&m).unwrap();
        assert_eq!(offered_paths(&probed), ["/stream.raw", "/wav/stream.wav"]);
        assert_eq!(app.speaker_settings(&m.id()).upnp_stream_profile, Some(StreamProfile::Wav));
        end(session);
        probed.lock().unwrap().offered.clear();
        let session = app.start_upnp_manual(&m).unwrap();
        assert_eq!(offered_paths(&probed), ["/wav/stream.wav"]);
        end(session);
    }

    /// A renderer that lists `audio/wav`, takes every URI, and answers
    /// Play with what `play` gives for the stream path it holds (None:
    /// it plays); `set` likewise for SetAVTransportURI.
    fn renderer_answering_by_path(
        set: impl Fn(&str) -> Option<crate::fake_renderer::Reply> + Send + Sync + 'static,
        play: impl Fn(&str) -> Option<crate::fake_renderer::Reply> + Send + Sync + 'static,
    ) -> crate::fake_renderer::FakeRenderer {
        use crate::fake_renderer::{renderer_description, FakeRenderer, Reply, AV_TRANSPORT, CONNECTION_MANAGER};
        let current = Mutex::new(String::new());
        FakeRenderer::start(move |req| match req.soap_action() {
            Some("GetProtocolInfo") => {
                Reply::soap_ok(CONNECTION_MANAGER, "GetProtocolInfo", &[("Source", ""), ("Sink", "http-get:*:audio/wav:*")])
            }
            Some("SetAVTransportURI") => {
                let path = url::Url::parse(&req.soap_arg("CurrentURI").unwrap()).unwrap().path().to_string();
                let answer = set(&path);
                if answer.is_none() {
                    *current.lock().unwrap() = path;
                }
                answer.unwrap_or_else(|| Reply::soap_ok(AV_TRANSPORT, "SetAVTransportURI", &[]))
            }
            Some("Play") => play(&current.lock().unwrap()).unwrap_or_else(|| Reply::soap_ok(AV_TRANSPORT, "Play", &[])),
            Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
            None if req.method == "GET" => Reply::ok(&renderer_description("Den Amp", &["ConnectionManager"])),
            None => Reply::raw("HTTP/1.1 200 OK\r\nSID: uuid:strict\r\nTIMEOUT: Second-1800\r\nContent-Length: 0\r\n\r\n"),
        })
    }

    /// The stream paths a renderer was given, in order.
    fn given_paths(fake: &crate::fake_renderer::FakeRenderer) -> Vec<String> {
        fake.requests()
            .iter()
            .filter(|r| r.soap_action() == Some("SetAVTransportURI"))
            .map(|r| url::Url::parse(&r.soap_arg("CurrentURI").unwrap()).unwrap().path().to_string())
            .collect()
    }

    #[test]
    fn a_refusal_about_the_transports_state_ends_the_connect() {
        use crate::fake_renderer::Reply;
        for code in [701, 705, 715, 718] {
            let fake = renderer_answering_by_path(|_| None, move |_| Some(Reply::soap_fault(code, "Not now")));
            let app = serving_app();
            let Err(msg) = app.start_upnp_manual(&manual_upnp(&fake)) else { panic!("{code}: connected") };
            assert_eq!(given_paths(&fake), ["/stream.raw"], "{code}");
            assert!(!fake.soap_actions().iter().any(|a| a == "GetProtocolInfo"), "{code}");
            assert!(msg.starts_with("Den Amp isn't ready to take the stream. Try again in a moment."), "{code}: {msg}");
            assert_eq!(app.speaker_settings(&manual_upnp(&fake).id()).upnp_stream_profile, None);
        }
    }

    #[test]
    fn a_refusal_naming_the_content_moves_on_and_what_plays_is_remembered() {
        use crate::fake_renderer::Reply;
        let fake = renderer_answering_by_path(|_| None, |path| (path == "/stream.raw").then(|| Reply::soap_fault(704, "Format not supported")));
        let app = serving_app();
        let m = manual_upnp(&fake);
        let session = app.start_upnp_manual(&m).expect("the second offer plays");
        assert_eq!(given_paths(&fake), ["/stream.raw", "/wav/stream.wav"]);
        assert_eq!(app.speaker_settings(&m.id()).upnp_stream_profile, Some(StreamProfile::Wav));
        end(session);
    }

    #[test]
    fn after_a_generic_refusal_what_plays_serves_this_session_only() {
        use crate::fake_renderer::Reply;
        for refusal in [Reply::status(500, "Internal Server Error", &[], ""), Reply::soap_fault(501, "Action Failed")] {
            let fake = renderer_answering_by_path(|_| None, move |path| (path == "/stream.raw").then(|| refusal.clone()));
            let app = serving_app();
            let m = manual_upnp(&fake);
            let session = app.start_upnp_manual(&m).expect("the second offer plays");
            assert_eq!(given_paths(&fake), ["/stream.raw", "/wav/stream.wav"]);
            let ActiveSession::Upnp(s) = &session.0 else { unreachable!() };
            assert_eq!(s.profile, StreamProfile::Wav);
            assert_eq!(app.speaker_settings(&m.id()), SpeakerSettings::default(), "nothing saved");
            end(session);
        }
    }

    #[test]
    fn the_original_stream_is_never_stored_and_a_refused_memory_is_cleared() {
        let fake = renderer_answering_by_path(|_| None, |_| None);
        let app = serving_app();
        let m = manual_upnp(&fake);
        // Plays at once: nothing to write.
        let session = app.start_upnp_manual(&m).unwrap();
        end(session);
        assert!(!app.user_config.lock().unwrap().speaker_settings.contains_key(&m.id()));

        // Remembered, then refused: the original stream plays, and the
        // memory goes.
        use crate::fake_renderer::Reply;
        let fake = renderer_answering_by_path(|path| (path == "/wav/stream.wav").then(|| Reply::soap_fault(714, "Illegal MIME-type")), |_| None);
        let m = manual_upnp(&fake);
        app.set_speaker_settings(&m.id(), SpeakerSettings { upnp_stream_profile: Some(StreamProfile::Wav), ..Default::default() });
        let session = app.start_upnp_manual(&m).unwrap();
        assert_eq!(given_paths(&fake), ["/wav/stream.wav", "/stream.raw"]);
        assert_eq!(app.speaker_settings(&m.id()).upnp_stream_profile, None);
        end(session);
    }

    #[test]
    fn a_refused_play_moves_on_to_the_next_profile() {
        use crate::fake_renderer::{renderer_description, FakeRenderer, Reply, AV_TRANSPORT};
        let current = Mutex::new(String::new());
        let fake = FakeRenderer::start(move |req| match req.soap_action() {
            Some("SetAVTransportURI") => {
                *current.lock().unwrap() = req.soap_arg("CurrentURI").unwrap();
                Reply::soap_ok(AV_TRANSPORT, "SetAVTransportURI", &[])
            }
            Some("Play") if !current.lock().unwrap().ends_with("/wav/stream.wav") => Reply::soap_fault(704, "Playing failed"),
            Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
            None if req.method == "GET" => Reply::ok(&renderer_description("Den Amp", &[])),
            None => Reply::raw("HTTP/1.1 200 OK\r\nSID: uuid:strict\r\nTIMEOUT: Second-1800\r\nContent-Length: 0\r\n\r\n"),
        });
        let app = serving_app();
        let session = app.start_upnp_manual(&manual_upnp(&fake)).expect("the second profile plays");
        assert_eq!(fake.soap_actions(), ["Stop", "SetAVTransportURI", "Play", "Stop", "SetAVTransportURI", "Play"]);
        end(session);
    }

    #[test]
    fn a_slow_speaker_holds_up_neither_the_window_nor_the_newest_level() {
        let app = test_app();
        let (url, got, release) = slow_soap_speaker();
        bind(&app, upnp_session(&url));
        let wait = Duration::from_secs(5);

        app.send_to_speaker(SpeakerCommand::Volume(31));
        assert_eq!(got.recv_timeout(wait), Ok(Soap::Volume(31)));
        // The speaker is sitting on that request. The window's per-repaint
        // session reads must not wait for it, and the drag goes on.
        assert!(app.session.try_lock().is_ok(), "session lock held across the speaker round trip");
        assert!(app.selected_speaker().is_some());
        for level in 32..=50 {
            app.send_to_speaker(SpeakerCommand::Volume(level));
        }
        release.send(()).unwrap();
        // Next comes the newest level, not 32, 33, …
        assert_eq!(got.recv_timeout(wait), Ok(Soap::Volume(50)));
        release.send(()).unwrap();
        assert!(got.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn the_window_slider_sets_the_speaker_and_the_windows_slider() {
        let app = test_app();
        let pushed = record_windows_pushes(&app);
        let (url, got, release) = slow_soap_speaker();
        bind(&app, upnp_session(&url));
        app.set_speaker_volume(64);
        assert_eq!(app.current_volume(), Some(64));
        assert_eq!(*pushed.lock().unwrap(), vec![(sonos_to_millibels(64), false)]);
        // Windows reports the level straight back: not a new change.
        assert_eq!(app.vsync.driver_changed(64), None);
        assert_eq!(got.recv_timeout(Duration::from_secs(5)), Ok(Soap::Volume(64)));
        release.send(()).unwrap();
        // The same level again still goes to the speaker, not to Windows.
        app.set_speaker_volume(64);
        assert_eq!(got.recv_timeout(Duration::from_secs(5)), Ok(Soap::Volume(64)));
        release.send(()).unwrap();
        assert_eq!(pushed.lock().unwrap().len(), 1);
    }

    /// Records what is pushed to the Windows slider.
    fn record_windows_pushes(app: &App) -> Arc<Mutex<Vec<(i32, bool)>>> {
        let pushed = Arc::new(Mutex::new(Vec::new()));
        let p = pushed.clone();
        app.set_windows_volume_push(move |mb, muted| {
            p.lock().unwrap().push((mb, muted));
            Ok(())
        });
        pushed
    }

    #[test]
    fn levels_queued_for_one_speaker_never_reach_the_next() {
        let app = test_app();
        let (url_a, got_a, release_a) = slow_soap_speaker();
        let (url_b, got_b, release_b) = slow_soap_speaker();
        let wait = Duration::from_secs(5);
        bind(&app, upnp_session(&url_a));
        app.send_to_speaker(SpeakerCommand::Volume(22));
        assert_eq!(got_a.recv_timeout(wait), Ok(Soap::Volume(22)));
        // A is slow; the user turns it up, hears nothing, and switches.
        app.send_to_speaker(SpeakerCommand::Volume(70));
        app.send_to_speaker(SpeakerCommand::Mute(true));
        bind(&app, upnp_session(&url_b));
        release_a.send(()).unwrap();
        assert!(got_a.recv_timeout(Duration::from_millis(300)).is_err());
        assert!(got_b.recv_timeout(Duration::from_millis(300)).is_err());
        // Changes made after the switch go to B.
        app.send_to_speaker(SpeakerCommand::Volume(40));
        assert_eq!(got_b.recv_timeout(wait), Ok(Soap::Volume(40)));
        release_b.send(()).unwrap();
    }

    #[test]
    fn moving_the_window_slider_unmutes() {
        let app = test_app();
        let pushed = record_windows_pushes(&app);
        let (url, got, release) = slow_soap_speaker();
        bind(&app, upnp_session(&url));
        app.vsync.note_muted(true);
        app.set_speaker_volume(30);
        assert!(!app.vsync.is_muted());
        assert_eq!(*pushed.lock().unwrap(), vec![(sonos_to_millibels(30), false)]);
        let wait = Duration::from_secs(5);
        assert_eq!(got.recv_timeout(wait), Ok(Soap::Volume(30)));
        release.send(()).unwrap();
        assert_eq!(got.recv_timeout(wait), Ok(Soap::Mute(false)));
        release.send(()).unwrap();
    }

    #[test]
    fn the_window_slider_does_nothing_without_a_speaker() {
        let app = test_app();
        let pushed = record_windows_pushes(&app);
        app.set_speaker_volume(30);
        assert_eq!(app.current_volume(), None);
        assert!(pushed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_new_speakers_level_moves_the_windows_slider() {
        let app = test_app();
        let pushed = record_windows_pushes(&app);
        let (url, _got, _release) = slow_soap_speaker();
        app.vsync.driver_changed(50);
        let number = {
            let mut guard = app.session.lock().unwrap();
            app.bind_session(&mut guard, upnp_session(&url))
        };
        app.speaker_level_on_connect(number, 20);
        assert_eq!(app.current_volume(), Some(20));
        assert_eq!(*pushed.lock().unwrap(), vec![(sonos_to_millibels(20), false)]);
        // A level read from a speaker no longer bound is ignored.
        bind(&app, upnp_session(&url));
        app.speaker_level_on_connect(number, 35);
        assert_eq!(app.current_volume(), Some(20));
        assert_eq!(pushed.lock().unwrap().len(), 1);
    }

    /// The connect step of `select_speaker_opts` for `session`: bind it
    /// and show the level it reports as it starts, if any.
    fn connect_with_level(app: &App, session: ActiveSession) {
        let level = session.level_on_connect();
        let number = {
            let mut guard = app.session.lock().unwrap();
            app.bind_session(&mut guard, session)
        };
        if let Some(level) = level {
            app.speaker_level_on_connect(number, level);
        }
    }

    #[test]
    fn a_sendspin_player_without_a_level_leaves_the_sliders_alone() {
        use crate::sendspin::server::test_player;
        let app = test_app();
        let pushed = record_windows_pushes(&app);
        app.vsync.driver_changed(50);
        // It takes the volume command but never says where it is.
        let (session, player) = test_player::connect(&["volume", "mute"], None);
        connect_with_level(&app, ActiveSession::Sendspin(session));
        assert_eq!(app.current_volume(), Some(50));
        assert!(pushed.lock().unwrap().is_empty());
        // A level it reports later is its level from then on.
        player.report(serde_json::json!({"volume": 40}));
        let reported = || match app.session.lock().unwrap().as_ref() {
            Some(ActiveSession::Sendspin(s)) => s.volume(),
            _ => None,
        };
        assert!(test_player::eventually(|| reported() == Some(40)));
    }

    #[test]
    fn a_sendspin_players_reported_level_moves_the_sliders() {
        use crate::sendspin::server::test_player;
        let app = test_app();
        let pushed = record_windows_pushes(&app);
        app.vsync.driver_changed(50);
        let (session, _player) = test_player::connect(&["volume"], Some(30));
        connect_with_level(&app, ActiveSession::Sendspin(session));
        assert_eq!(app.current_volume(), Some(30));
        assert_eq!(*pushed.lock().unwrap(), vec![(sonos_to_millibels(30), false)]);
    }

    #[test]
    fn where_the_speaker_settles_reaches_the_windows_slider() {
        let app = test_app();
        let pushed = record_windows_pushes(&app);
        let (url, got, release) = slow_soap_speaker();
        bind(&app, upnp_session(&url));
        let wait = Duration::from_secs(5);
        for level in [30, 60] {
            assert_eq!(app.vsync.driver_changed(level), Some(level));
            app.send_to_speaker(SpeakerCommand::Volume(level));
            assert_eq!(got.recv_timeout(wait), Ok(Soap::Volume(level)));
            release.send(()).unwrap();
        }
        // The speaker's last word names 30: it stopped there.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(app.vsync.sonos_changed(30), None);
        let deadline = Instant::now() + wait;
        while pushed.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(*pushed.lock().unwrap(), vec![(sonos_to_millibels(30), false)]);
        assert_eq!(app.current_volume(), Some(30));
    }

    #[test]
    fn privacy_off_allows_everyone() {
        let app = test_app();
        app.stream_gate.set_privacy(false);
        assert_eq!(app.check_stream_client(ip("192.168.1.99")), Ok(()));
        assert!(app.current_error().is_none());
    }

    #[test]
    fn privacy_on_refuses_strangers_admits_loopback() {
        let app = test_app();
        assert_eq!(app.check_stream_client(ip("192.168.1.99")), Err(Refusal::NotGranted));
        assert_eq!(app.check_stream_client(ip("127.0.0.1")), Ok(()));
        assert_eq!(app.check_stream_client(ip("::1")), Ok(()));
        // The advertised IP is NOT this test host's address, so it is
        // not admitted as "this machine" (the proxy/NAT case).
        assert_eq!(app.check_stream_client(ip("192.168.1.10")), Err(Refusal::NotGranted));
    }

    #[test]
    fn grant_lives_and_dies_with_its_holder() {
        let app = test_app();
        let grant = app.stream_gate.grant([ip("192.168.1.50")]);
        assert_eq!(app.check_stream_client(ip("192.168.1.50")), Ok(()));
        assert_eq!(app.check_stream_client(ip("::ffff:192.168.1.50")), Ok(()));
        assert_eq!(app.check_stream_client(ip("192.168.1.51")), Err(Refusal::NotGranted));
        drop(grant);
        assert_eq!(app.check_stream_client(ip("192.168.1.50")), Err(Refusal::NotGranted));
    }

    #[test]
    fn sendspin_discovery_pauses_with_the_discovery_adapter() {
        let app = test_app();
        {
            let mut uc = app.user_config.lock().unwrap();
            uc.sendspin.enabled = true;
            uc.discovery_adapter =
                Some(SavedAdapter { key: "{no-such-adapter}".into(), name: "USB LAN".into() });
        }
        app.sendspin.discovery_allowed.store(true, Ordering::SeqCst);
        // The chosen adapter is missing: discovery pauses, Sendspin's too.
        app.refresh_discovery_scope();
        assert_eq!(app.current_discovery_scope(), Resolved::Paused { name: "USB LAN".into() });
        {
            let browse = app.sendspin.browse.lock().unwrap();
            let ctl = browse.as_ref().expect("Sendspin discovery is on");
            assert_eq!(ctl.applied(), Some(discovery_net::MdnsScope::Paused));
            assert!(!ctl.is_running());
        }
        let found = app.sendspin.discovery.lock().unwrap().clone().expect("Sendspin discovery is on");
        assert!(found.renderers().is_empty());
        assert!(app.sendspin_speakers(None).is_empty());

        // Sendspin off: its discovery goes away entirely.
        app.user_config.lock().unwrap().sendspin.enabled = false;
        app.sync_sendspin_discovery();
        assert!(app.sendspin.discovery.lock().unwrap().is_none());
        assert!(app.sendspin.browse.lock().unwrap().is_none());
    }

    #[test]
    fn legacy_launch_preferences_cannot_move_the_sound() {
        use crate::sendspin_app::{LaunchSkip, SendspinLaunch, MA_OUTPUT_ID};
        let app = test_app();
        for id in ["sendspin:kitchen", MA_OUTPUT_ID] {
            for setting in [AutoConnect::Default, AutoConnect::Always, AutoConnect::Never] {
                let mut cfg = app.user_config.lock().unwrap();
                cfg.last_speaker_id = Some(id.into());
                cfg.auto_reconnect_on_launch = true;
                cfg.sendspin.enabled = true;
                cfg.speaker_settings.entry(id.into()).or_default().auto_connect = setting;
                drop(cfg);
                assert_eq!(app.sendspin_launch_plan(), Some(SendspinLaunch::Skip(LaunchSkip::ReconnectOff)));
            }
        }
    }

    #[test]
    fn music_assistant_is_listed_while_a_server_is_connected_or_it_is_the_output() {
        use crate::sendspin::source::SourceState;
        use crate::sendspin_app::MA_OUTPUT_ID;
        let app = test_app();
        // A server was paired before: that alone doesn't list it.
        app.user_config.lock().unwrap().sendspin.server_pairings.push(crate::sendspin::store::ServerPairing {
            server_id: "server".into(),
            psk: "00".into(),
            server_name: Some("Server".into()),
            last_used_unix: 0,
        });
        app.set_sendspin_source_enabled(true).unwrap();
        let listed = || app.speaker_view().speakers.iter().any(|s| s.id == MA_OUTPUT_ID);
        assert!(!listed(), "listed before a server connected");
        app.set_sendspin_source_state(SourceState::Connected { server: "Server".into(), streaming: false });
        assert!(listed());
        // A server that only asks to pair isn't one that can play.
        app.set_sendspin_source_state(SourceState::NeedsPairing { server: "Server".into() });
        assert!(!listed());
        app.set_sendspin_source_state(SourceState::Waiting);
        assert!(!listed(), "listed after the server went away");
        // As the output in use it stays listed without a server.
        assert!(app.sendspin_speakers(Some(MA_OUTPUT_ID)).iter().any(|s| s.id == MA_OUTPUT_ID));
    }

    #[test]
    fn sendspin_speakers_are_listed_while_enabled() {
        use crate::sendspin::server::{SendspinDiscoveryState, SendspinRenderer};
        let app = test_app();
        let found = SendspinDiscoveryState::new();
        found.insert(SendspinRenderer {
            instance: "Kitchen".into(),
            friendly_name: "Kitchen".into(),
            ip: ip("192.168.1.40"),
            port: 8928,
            path: "/sendspin".into(),
        });
        *app.sendspin.discovery.lock().unwrap() = Some(found);
        let listed = || app.speaker_view().speakers.iter().any(|s| s.id == "sendspin:kitchen");
        assert!(!listed(), "listed while Sendspin is off");
        app.user_config.lock().unwrap().sendspin.enabled = true;
        assert!(listed());
    }

    #[test]
    fn failed_airplay_discovery_is_retried_until_it_starts() {
        let app = test_app();
        // A browse that can't start until `fail` is cleared.
        let fail = Arc::new(AtomicBool::new(true));
        let f = fail.clone();
        let ctl = MdnsController::stub(
            "AirPlay",
            move || {
                if f.load(Ordering::SeqCst) {
                    Err(anyhow!("no socket"))
                } else {
                    Ok(())
                }
            },
            || {},
        );
        let failed = ctl.apply(&app.current_discovery_scope()).unwrap_err();
        app.note_discovery_failure(&failed);
        *app.mdns.lock().unwrap() = Some(ctl);
        let err = app.airplay_discovery_error().expect("failure noted");
        assert!(err.contains("no socket"), "{}", err);
        assert_eq!(app.discovery_net_status().error, Some(err.clone()));

        // The network check retries while the scope stays the same, and
        // says nothing more while the browse still can't start.
        app.dismiss_error();
        app.refresh_discovery_scope();
        assert_eq!(app.airplay_discovery_error(), Some(err));
        assert_eq!(app.current_error(), None);
        assert!(!app.mdns.lock().unwrap().as_ref().unwrap().is_running());
        fail.store(false, Ordering::SeqCst);
        app.refresh_discovery_scope();
        assert_eq!(app.airplay_discovery_error(), None);
        assert_eq!(app.discovery_net_status().error, None);
        assert!(app.mdns.lock().unwrap().as_ref().unwrap().is_running());
    }

    #[test]
    fn failed_sendspin_discovery_is_shown_and_retried() {
        use crate::sendspin::server::SendspinDiscoveryState;
        let app = test_app();
        app.user_config.lock().unwrap().sendspin.enabled = true;
        app.sendspin.discovery_allowed.store(true, Ordering::SeqCst);
        // A browse that can't start until `fail` is cleared.
        let fail = Arc::new(AtomicBool::new(true));
        let f = fail.clone();
        *app.sendspin.discovery.lock().unwrap() = Some(SendspinDiscoveryState::new());
        *app.sendspin.browse.lock().unwrap() = Some(MdnsController::stub(
            "Sendspin",
            move || {
                if f.load(Ordering::SeqCst) {
                    Err(anyhow!("no socket"))
                } else {
                    Ok(())
                }
            },
            || {},
        ));
        app.sync_sendspin_discovery();
        let err = app.sendspin_discovery_error().expect("failure noted");
        assert!(err.contains("no socket"), "{}", err);
        assert_eq!(app.current_error(), Some(err.clone()));
        assert_eq!(app.discovery_net_status().error, Some(err.clone()));
        match app.start_sendspin_player("sendspin:kitchen", false) {
            Err(e) => assert_eq!(e, "Sendspin discovery is not running"),
            Ok(_) => panic!("connected without discovery"),
        }

        // The network check retries while the scope stays the same,
        // without repeating the banner.
        app.dismiss_error();
        app.refresh_discovery_scope();
        assert!(app.sendspin_discovery_error().is_some());
        assert_eq!(app.current_error(), None);
        fail.store(false, Ordering::SeqCst);
        app.refresh_discovery_scope();
        assert_eq!(app.sendspin_discovery_error(), None);
        assert_eq!(app.discovery_net_status().error, None);
        assert!(app.sendspin.browse.lock().unwrap().as_ref().unwrap().is_running());
        match app.start_sendspin_player("sendspin:kitchen", false) {
            Err(e) => assert!(e.starts_with("no Sendspin speaker with id"), "{}", e),
            Ok(_) => panic!("connected to a speaker that wasn't found"),
        }

        app.user_config.lock().unwrap().sendspin.enabled = false;
        app.sync_sendspin_discovery();
        assert!(app.sendspin.browse.lock().unwrap().is_none());
    }

    #[test]
    fn a_connect_decided_before_a_user_pick_is_skipped() {
        let app = test_app();
        let seen = app.user_pick_count();
        assert!(app.select_speaker("sendspin:nowhere").is_err());
        assert!(!app.is_speaker_bound() && app.connecting_to().is_none());
        app.dismiss_error();
        app.connect_unless_user_picked("sendspin:kitchen", seen);
        assert_eq!(app.connecting_to(), None);
        assert_eq!(app.current_error(), None);
    }

    #[test]
    fn refusal_is_surfaced_once_per_peer() {
        let app = test_app();
        assert_eq!(app.check_stream_client(ip("192.168.1.99")), Err(Refusal::NotGranted));
        let msg = app.current_error().expect("refusal recorded as a user-visible error");
        assert!(msg.contains("192.168.1.99"), "{msg}");
        assert!(msg.contains("can be allowed on the speaker's page"), "{msg}");
        // A repeat within the report interval is refused silently.
        app.dismiss_error();
        assert_eq!(app.check_stream_client(ip("192.168.1.99")), Err(Refusal::NotGranted));
        assert!(app.current_error().is_none());
    }

    fn assert_cut(rx: &crossbeam_channel::Receiver<crate::http_server::PcmFrame>, who: &str) {
        assert!(
            matches!(rx.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)),
            "{who} still connected"
        );
    }

    fn publish(app: &App) {
        app.hub.publish(crate::http_server::PcmFrame(Arc::new(vec![0u8; 4])));
    }

    #[test]
    fn disabling_streaming_cuts_every_network_listener_in_both_modes() {
        for privacy in [false, true] {
            let app = test_app();
            app.stream_gate.set_privacy(privacy);
            // The speaker being played on, as its session's grant admits it.
            let _grant = app.stream_gate.grant([ip("192.168.1.50")]);
            let stranger = app.hub.subscribe_from(ip("192.168.1.99"));
            let speaker = app.hub.subscribe_from(ip("192.168.1.50"));
            let this_pc = app.hub.subscribe_from(ip("127.0.0.1"));
            let sender = app.hub.subscribe(); // an AirPlay sender, in-process
            assert_eq!(app.check_stream_client(ip("192.168.1.50")), Ok(()));
            let verdict = if privacy { Err(Refusal::NotGranted) } else { Ok(()) };
            assert_eq!(app.check_stream_client(ip("192.168.1.99")), verdict);
            app.dismiss_error();

            app.set_streaming_enabled(false).unwrap();
            publish(&app);
            assert_cut(&stranger, "stranger");
            assert_cut(&speaker, "speaker");
            assert!(this_pc.try_recv().is_ok(), "privacy {privacy}");
            assert!(sender.try_recv().is_ok(), "privacy {privacy}");
            for other in ["192.168.1.99", "192.168.1.50", "::ffff:192.168.1.50"] {
                let verdict = app.check_stream_client(ip(other));
                assert_eq!(verdict, Err(Refusal::StreamingOff), "{other}, privacy {privacy}");
            }
            assert_eq!(app.check_stream_client(ip("::1")), Ok(()));
            // Closing the stream is what Disable is for: no message about it.
            assert!(app.current_error().is_none(), "{:?}", app.current_error());

            app.set_streaming_enabled(true).unwrap();
            assert_eq!(app.check_stream_client(ip("192.168.1.50")), Ok(()));
            let verdict = if privacy { Err(Refusal::NotGranted) } else { Ok(()) };
            assert_eq!(app.check_stream_client(ip("192.168.1.99")), verdict);
        }
    }

    #[test]
    fn no_discovery_still_serves_the_stream_to_the_network() {
        // `--no-discovery` serves HTTP only: no speaker is picked, and
        // players elsewhere on the network fetch the stream themselves.
        // That relies on streaming starting enabled and staying so until
        // the user disables it.
        let app = test_app();
        assert!(app.config.no_discovery && app.discovery.is_none());
        app.stream_gate.set_privacy(false);
        assert!(app.is_streaming_enabled());
        assert_eq!(app.check_stream_client(ip("192.168.1.99")), Ok(()));
        app.set_streaming_enabled(false).unwrap();
        assert_eq!(app.check_stream_client(ip("192.168.1.99")), Err(Refusal::StreamingOff));
        app.set_streaming_enabled(true).unwrap();
        assert_eq!(app.check_stream_client(ip("192.168.1.99")), Ok(()));
        // With privacy mode on there is no speaker to grant: this PC only.
        app.stream_gate.set_privacy(true);
        assert_eq!(app.check_stream_client(ip("192.168.1.99")), Err(Refusal::NotGranted));
        assert_eq!(app.check_stream_client(ip("127.0.0.1")), Ok(()));
    }

    /// An app run without `--advertise-ip`, its web UI on.
    fn app_without_advertise_ip(bind: &str) -> Arc<App> {
        let mut config = App::test_config();
        config.advertise_ip = None;
        config.bind = bind.parse().unwrap();
        let app = App::with_settings(config, UserConfig::in_memory(), None, None);
        app.set_web_ui_enabled(true).unwrap();
        app
    }

    fn link_host(app: &App) -> String {
        let link = app.web_link().expect("the web UI is on");
        let rest = link.strip_prefix("http://").unwrap();
        rest[..rest.find("/r/").unwrap()].to_string()
    }

    /// A UPnP session whose speaker takes requests and never answers:
    /// stopping it waits out the client's timeout. The receiver hears each
    /// request's first line.
    fn unanswering_upnp_session() -> (ActiveSession, crossbeam_channel::Receiver<String>) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/AVTransport/Control", listener.local_addr().unwrap());
        let (got_tx, got_rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut buf = [0u8; 2048];
                let n = conn.read(&mut buf).unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let soap_action = text.lines().find(|l| l.to_ascii_lowercase().starts_with("soapaction"));
                let _ = got_tx.send(soap_action.unwrap_or_default().to_string());
                held.push(conn);
            }
        });
        let mut session = upnp_session("");
        if let ActiveSession::Upnp(s) = &mut session {
            s.renderer.av_transport_control_url = url;
        }
        (session, got_rx)
    }

    #[test]
    fn quitting_tells_the_speaker_but_doesnt_wait_out_one_that_never_answers() {
        let app = test_app();
        let (session, got) = unanswering_upnp_session();
        bind(&app, session);
        let started = Instant::now();
        app.shut_down_within(Duration::from_millis(100), Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(2), "quit took {:?}", started.elapsed());
        let stop = got.recv_timeout(Duration::from_secs(5)).expect("the speaker was never told");
        assert!(stop.contains("#Stop"), "{stop}");
        assert!(!app.is_speaker_bound());
        // A connect still in flight can't bind after this: it stops what it
        // set up instead (it binds only while streaming is enabled).
        assert!(!app.is_streaming_enabled());
    }

    #[test]
    fn a_pick_from_the_web_while_quitting_is_refused() {
        let app = test_app();
        app.request_shutdown();
        app.shut_down_within(Duration::from_millis(10), Duration::from_millis(10));
        let refused = app.select_speaker_from_remote("uuid:kitchen").unwrap_err();
        assert!(refused.contains("quitting"), "{refused}");
        // And it didn't turn streaming back on for a session to bind.
        assert!(!app.is_streaming_enabled());
    }

    #[test]
    fn headless_shutdown_waits_for_the_hook_and_has_a_deadline() {
        let app = test_app();
        let (started, entered) = crossbeam_channel::bounded(1);
        let (release, proceed) = crossbeam_channel::bounded(1);
        app.on_quit(move || {
            started.send(()).unwrap();
            let _ = proceed.recv();
        });
        let quitting = app.clone();
        let quit = std::thread::spawn(move || quitting.request_shutdown());
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let before = Instant::now();
        let bound = Duration::from_millis(40);
        app.wait_for_headless_shutdown(bound);
        assert!(before.elapsed() >= bound);
        let (done, finished) = crossbeam_channel::bounded(1);
        let waiting = app.clone();
        let headless = std::thread::spawn(move || {
            waiting.wait_for_headless_shutdown(Duration::from_secs(4));
            done.send(()).unwrap();
        });
        assert!(finished.recv_timeout(bound).is_err());
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
        headless.join().unwrap();
        quit.join().unwrap();
    }

    #[test]
    fn quitting_runs_the_quit_hook_once() {
        let app = test_app();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        app.on_quit(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        app.request_shutdown();
        app.request_shutdown();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn quitting_waits_a_moment_for_a_connect_in_flight() {
        let app = test_app();
        *app.connecting.lock().unwrap() = Some("Kitchen".into());
        let finishing = {
            let app = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                *app.connecting.lock().unwrap() = None;
            })
        };
        let started = Instant::now();
        app.shut_down_within(Duration::from_secs(30), Duration::from_secs(30));
        let took = started.elapsed();
        finishing.join().unwrap();
        assert!(took >= Duration::from_millis(50), "didn't wait for the connect ({took:?})");
        assert!(took < Duration::from_secs(5), "waited {took:?}");
        // One that doesn't finish is waited for only so long.
        *app.connecting.lock().unwrap() = Some("Kitchen".into());
        let started = Instant::now();
        app.shut_down_within(Duration::from_millis(100), Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(2), "waited {:?}", started.elapsed());
    }

    #[test]
    fn removing_a_speaker_added_by_address_forgets_everything_kept_for_it() {
        let app = test_app();
        let id = app.add_manual_speaker(ManualKind::AirPlay, "Kitchen", "192.168.1.30", "").unwrap();
        let mut settings = app.speaker_settings(&id);
        settings.hidden = true;
        settings.auto_connect = AutoConnect::Always;
        app.set_speaker_settings(&id, settings);
        app.set_airplay_password(&id, "secret");
        app.store_airplay_pairing(&id, some_pairing("k"));
        // Sealed copies this account can't open (a file from another PC).
        let locked = |key: &str| crate::secret_store::LockedNode {
            pointer: Some(crate::secret_store::pointer(&[key.to_string(), id.clone()])),
            sealed: serde_json::json!("opaque"),
        };
        app.user_config.lock().unwrap().locked_secrets =
            vec![locked("airplay_passwords"), locked("airplay_pairings")];

        app.remove_manual_speaker(&id);
        {
            let uc = app.user_config.lock().unwrap();
            assert!(uc.locked_secrets.is_empty(), "{:?}", uc.locked_secrets);
            assert!(!uc.airplay_passwords.contains_key(&id));
            assert!(!uc.airplay_pairings.contains_key(&id));
        }
        // Added again: a new speaker, listed and with default settings.
        assert_eq!(app.add_manual_speaker(ManualKind::AirPlay, "Kitchen", "192.168.1.30", "").unwrap(), id);
        assert!(app.speaker_settings(&id).is_default());
        assert!(app.speaker_view().speakers.iter().any(|s| s.id == id));
    }

    #[test]
    fn a_connect_that_ends_after_its_speaker_was_removed_is_undone() {
        let app = test_app();
        let id = app
            .add_manual_speaker(ManualKind::Upnp, "Den", "http://192.168.1.31:49152/desc.xml", "")
            .unwrap();
        // A connect to it is under way when it is removed…
        *app.connecting.lock().unwrap() = Some("Den".into());
        app.remove_manual_speaker(&id);
        // …and binds (and saves it as the last speaker) before it ends.
        let (mut session, got) = unanswering_upnp_session();
        if let ActiveSession::Upnp(s) = &mut session {
            s.id_override = Some(id.clone());
        }
        bind(&app, session);
        app.user_config.lock().unwrap().last_speaker_id = Some(id.clone());
        *app.last_speaker_id.lock().unwrap() = Some(id.clone());
        *app.connecting.lock().unwrap() = None;
        assert!(eventually(|| !app.is_speaker_bound()), "still playing on a removed speaker");
        assert!(got.recv_timeout(Duration::from_secs(5)).is_ok_and(|a| a.contains("#Stop")));
        assert_eq!(app.saved_speaker_id(), None);
        assert_eq!(*app.last_speaker_id.lock().unwrap(), None);
    }

    #[test]
    fn the_windows_device_name_follows_the_speaker_being_played_on() {
        let app = test_app();
        let names = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let record = names.clone();
        app.follow_endpoint_label_every(
            move |name| record.lock().unwrap().push(name.map(str::to_string)),
            Duration::from_millis(5),
        );
        let seen = |n: usize| eventually(|| names.lock().unwrap().len() >= n);
        // At start: back to the plain name, whatever a previous run left.
        assert!(seen(1));
        bind(&app, upnp_session(""));
        assert!(seen(2));
        // Disable takes the session: the plain name again.
        app.set_streaming_enabled(false).unwrap();
        assert!(seen(3));
        std::thread::sleep(Duration::from_millis(50));
        app.request_shutdown();
        assert_eq!(*names.lock().unwrap(), [None, Some("Kitchen".to_string()), None]);
    }

    /// A listed speaker for the ranking table.
    fn cand(id: &str, address: &str, name: &str) -> PlayerCandidate {
        PlayerCandidate {
            id: id.into(),
            address: address.into(),
            names: vec![name.into()],
            group_rooms: Vec::new(),
            device_name: None,
            kind: PlayerKind::Other,
            manual: false,
            hidden: false,
            used_before: false,
        }
    }

    fn upnp(id: &str, address: &str, name: &str) -> PlayerCandidate {
        PlayerCandidate { kind: PlayerKind::Upnp, ..cand(id, address, name) }
    }

    fn airplay(id: &str, address: &str, name: &str) -> PlayerCandidate {
        PlayerCandidate { kind: PlayerKind::AirPlay, ..cand(id, address, name) }
    }

    fn picked(query: &str, rows: &[PlayerCandidate]) -> Option<(String, PlayerMatchKind, usize)> {
        rank_player_match(query, rows).map(|m| (m.id, m.how, m.also_fitted))
    }

    #[test]
    fn player_ranks_a_full_name_above_part_of_one() {
        let rows = [cand("a", "10.0.0.1", "Kitchen Speaker"), cand("b", "10.0.0.2", "Kitchen")];
        assert_eq!(picked("kitchen", &rows), Some(("b".into(), PlayerMatchKind::FullName, 0)));
        assert_eq!(picked("  KITCHEN ", &rows), Some(("b".into(), PlayerMatchKind::FullName, 0)));
        // An address beats a name that happens to be the same text.
        let rows = [cand("a", "10.0.0.1", "10.0.0.2"), cand("b", "10.0.0.2", "Den")];
        assert_eq!(picked("10.0.0.2", &rows), Some(("b".into(), PlayerMatchKind::Address, 0)));
    }

    #[test]
    fn player_reaches_a_hidden_speaker_only_by_its_full_name_or_address() {
        let mut den = cand("den", "10.0.0.31:7000", "Den");
        den.hidden = true;
        let rows = [den, cand("kitchen", "10.0.0.30", "Kitchen")];
        assert_eq!(picked("den", &rows), Some(("den".into(), PlayerMatchKind::FullName, 0)));
        assert_eq!(picked("10.0.0.31", &rows), Some(("den".into(), PlayerMatchKind::Address, 0)));
        assert_eq!(picked("de", &rows), None);
        // A shown speaker of the same name comes first.
        let mut hidden = cand("hidden", "10.0.0.40", "Office");
        hidden.hidden = true;
        let rows = [hidden, cand("shown", "10.0.0.41", "office")];
        assert_eq!(picked("Office", &rows), Some(("shown".into(), PlayerMatchKind::FullName, 1)));
    }

    #[test]
    fn player_prefers_a_speaker_used_before() {
        let mut used = cand("used", "10.0.0.2", "Kitchen 2");
        used.used_before = true;
        let rows = [cand("new", "10.0.0.1", "Kitchen 1"), used];
        assert_eq!(picked("kitchen", &rows), Some(("used".into(), PlayerMatchKind::PartOfName, 1)));

    }

    #[test]
    fn player_takes_the_first_in_list_order_among_equals() {
        let rows = [cand("one", "10.0.0.1", "Kitchen 1"), cand("two", "10.0.0.2", "Kitchen 2")];
        assert_eq!(picked("kitchen", &rows), Some(("one".into(), PlayerMatchKind::PartOfName, 1)));
        assert_eq!(picked("bathroom", &rows), None);
        assert_eq!(picked("", &rows), None, "an empty name is no search");
    }

    /// A Sonos group row as the list builds it: labelled by its room and
    /// another, with the device's own description name behind it.
    fn sonos_group() -> PlayerCandidate {
        let mut g = upnp("group", "192.168.1.10", "Living Room + 2");
        g.names.push("Living Room".into());
        g.group_rooms = vec!["Kitchen".into(), "Patio".into()];
        g.device_name = Some("192.168.1.10 - Sonos One".into());
        g
    }

    #[test]
    fn player_finds_a_room_inside_a_sonos_group_then_its_device_name() {
        let rows = [sonos_group(), cand("den", "10.0.0.2", "Den")];
        assert_eq!(picked("Living Room", &rows), Some(("group".into(), PlayerMatchKind::FullName, 0)));
        assert_eq!(picked("living", &rows), Some(("group".into(), PlayerMatchKind::PartOfName, 0)));
        assert_eq!(picked(" pati ", &rows), Some(("group".into(), PlayerMatchKind::GroupRoom, 0)));
        assert_eq!(picked("Sonos One", &rows), Some(("group".into(), PlayerMatchKind::DeviceName, 0)));
        let mut hidden = rows.clone();
        hidden[0].hidden = true;
        assert_eq!(picked("Patio", &hidden), None);
        assert_eq!(picked("Sonos One", &hidden), None);
        assert_eq!(picked("Living Room", &hidden), Some(("group".into(), PlayerMatchKind::FullName, 0)));
    }

    #[test]
    fn player_finds_what_the_old_upnp_lookup_found() {
        // The UPnP lookup --player used before matched a renderer by its
        // exact address, part of its description name, part of its room,
        // part of a room in its group. Each still finds it.
        let mut alone = upnp("alone", "192.168.1.20", "192.168.1.20 - Sonos Five");
        alone.names.push("Bedroom".into());
        let rows = [sonos_group(), alone];
        for (query, id) in [
            ("192.168.1.20", "alone"),
            ("sonos five", "alone"),
            ("bedroom", "alone"),
            ("bed", "alone"),
            ("192.168.1.10", "group"),
            ("one", "group"),
            ("living", "group"),
            ("kitchen", "group"),
        ] {
            assert_eq!(picked(query, &rows).map(|p| p.0).as_deref(), Some(id), "{query}");
        }
    }

    #[test]
    fn player_matches_an_address_with_addresses_only() {
        // A Sonos's UPnP name holds its address; the speaker meant is off.
        let rows = [upnp("sonos", "192.168.1.50", "192.168.1.50 - Sonos One"), cand("odd", "10.0.0.9", "192.168.1.5")];
        assert_eq!(picked("192.168.1.5", &rows), None);
        assert_eq!(picked("192.168.1.50", &rows).map(|p| p.0).as_deref(), Some("sonos"));
        // A speaker added by address is listed with its port; either form finds it.
        let rows = [cand("manual", "192.168.1.40:7000", "Den"), cand("other", "192.168.1.41:7000", "Office")];
        assert_eq!(picked("192.168.1.40", &rows).map(|p| p.0).as_deref(), Some("manual"));
        assert_eq!(picked("192.168.1.40:7000", &rows).map(|p| p.0).as_deref(), Some("manual"));
        assert_eq!(picked("192.168.1.40:7001", &rows), None);
    }

    #[test]
    fn player_takes_a_sonoss_upnp_entry_before_its_airplay_one() {
        // List order would put either first; the UPnP entry wins.
        let rows = [airplay("airplay:SONOS", "192.168.1.30", "Kitchen"), upnp("uuid:SONOS", "192.168.1.30", "Kitchen")];
        assert_eq!(picked("Kitchen", &rows), Some(("uuid:SONOS".into(), PlayerMatchKind::FullName, 1)));
        assert_eq!(picked("192.168.1.30", &rows), Some(("uuid:SONOS".into(), PlayerMatchKind::Address, 1)));
        // An AirPlay speaker without a UPnP twin is taken as it is.
        let rows = [airplay("airplay:POD", "192.168.1.31", "Kitchen"), upnp("uuid:OTHER", "192.168.1.32", "Kitchen")];
        assert_eq!(picked("Kitchen", &rows).map(|p| p.0).as_deref(), Some("airplay:POD"));
    }

    #[test]
    fn player_waits_for_discoverys_first_look_before_choosing() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        // A Sonos answers mDNS first: its AirPlay entry is listed alone
        // until the first UPnP sweep is published.
        let mut steps = WishSteps::new(t0);
        let early = [airplay("airplay:SONOS", "192.168.1.30", "Kitchen")];
        assert_eq!(steps.next(at(800), "Kitchen", &early, false, false), None);
        let both = [airplay("airplay:SONOS", "192.168.1.30", "Kitchen"), upnp("uuid:SONOS", "192.168.1.30", "Kitchen")];
        assert_eq!(steps.next(at(3500), "Kitchen", &both, true, false).map(|m| m.id).as_deref(), Some("uuid:SONOS"));

        // A near match listed early doesn't win for good.
        let mut steps = WishSteps::new(t0);
        let early = [airplay("airplay:NEAR", "192.168.1.40", "Kitchen Speaker")];
        assert_eq!(steps.next(at(800), "Kitchen", &early, false, false), None);
        let later = [airplay("airplay:NEAR", "192.168.1.40", "Kitchen Speaker"), upnp("uuid:K", "192.168.1.41", "Kitchen")];
        assert_eq!(steps.next(at(3500), "Kitchen", &later, true, false).map(|m| m.id).as_deref(), Some("uuid:K"));

        // No UPnP speakers on the network: after the first look's time the
        // AirPlay speaker is taken.
        let mut steps = WishSteps::new(t0);
        let only = [airplay("airplay:POD", "192.168.1.31", "Kitchen")];
        assert_eq!(steps.next(at(4900), "Kitchen", &only, false, false), None);
        assert_eq!(steps.next(at(5000), "Kitchen", &only, false, false).map(|m| m.id).as_deref(), Some("airplay:POD"));

        // A speaker added by address is taken at once.
        let mut steps = WishSteps::new(t0);
        let manual = [PlayerCandidate { manual: true, ..cand("manual:den", "192.168.1.50:7000", "Den") }];
        assert_eq!(steps.next(at(1), "Den", &manual, false, false).map(|m| m.id).as_deref(), Some("manual:den"));
    }

    #[test]
    fn player_tries_a_speaker_once_per_appearance() {
        let t0 = Instant::now();
        let mut steps = WishSteps::new(t0);
        let rows = [upnp("uuid:K", "192.168.1.41", "Kitchen")];
        assert!(steps.next(t0, "Kitchen", &rows, true, true).is_none(), "nothing while a connect is under way");
        assert!(steps.next(t0, "Kitchen", &rows, true, false).is_some());
        assert!(steps.next(t0, "Kitchen", &rows, true, false).is_none(), "tried already");
        assert!(steps.next(t0, "Kitchen", &[], true, false).is_none());
        assert!(steps.next(t0, "Kitchen", &rows, true, false).is_some(), "back in the list: tried again");
    }

    #[test]
    fn a_speaker_that_wants_pin_pairing_is_said_to_need_the_person() {
        let reasons = [
            PinReason::Required,
            PinReason::Damaged("bad hex".into()),
            PinReason::NotProven { addr: "192.168.1.9".parse().unwrap(), detail: "M2".into() },
            PinReason::Refused,
        ];
        for reason in reasons {
            let text = pin_pairing_message("Living Room", &reason, false);
            assert!(needs_the_person(&text), "{reason:?}: {text}");
        }
        assert!(!needs_the_person("couldn't read http://127.0.0.1:9/desc.xml: Connection refused"));
    }

    #[test]
    fn a_partial_or_shared_player_match_is_explained_in_the_log() {
        let m = |how, also_fitted| PlayerMatch { id: "x".into(), how, also_fitted };
        assert_eq!(m(PlayerMatchKind::FullName, 0).explain("Kitchen", "Kitchen"), None);
        assert_eq!(m(PlayerMatchKind::Address, 0).explain("10.0.0.2", "Den"), None);
        assert_eq!(
            m(PlayerMatchKind::PartOfName, 2).explain("kit", "Kitchen").as_deref(),
            Some("--player \"kit\": taking \"Kitchen\", matched by part of its name; 2 other speaker(s) matched the same way")
        );
        assert!(m(PlayerMatchKind::FullName, 1).explain("Den", "Den").unwrap().contains("1 other"));
        assert!(m(PlayerMatchKind::GroupRoom, 0).explain("Patio", "Living Room + 2").unwrap().contains("Sonos group"));
    }

    #[test]
    fn a_player_wish_skips_a_hidden_near_match_and_takes_the_full_name() {
        let app = test_app();
        let near = app
            .add_manual_speaker(ManualKind::Upnp, "Den Speaker", "http://127.0.0.1:9/near.xml", "")
            .unwrap();
        let mut hide = app.speaker_settings(&near);
        hide.hidden = true;
        app.set_speaker_settings(&near, hide);
        app.connect_when_listed_every("Den".into(), Duration::from_millis(5));
        // Only the hidden near match is listed: no attempt.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(app.current_error(), None);
        // The speaker by that full name appears: that one is tried.
        app.add_manual_speaker(ManualKind::Upnp, "Den", "http://127.0.0.1:9/den.xml", "").unwrap();
        assert!(
            eventually(|| app.current_error().is_some_and(|e| e.contains("den.xml"))),
            "{:?}",
            app.current_error()
        );
    }

    #[test]
    fn player_finds_any_kind_of_speaker_by_name_or_address() {
        let app = test_app();
        let kitchen = app.add_manual_speaker(ManualKind::AirPlay, "Kitchen", "192.168.1.30", "").unwrap();
        let den = app
            .add_manual_speaker(ManualKind::Upnp, "Den", "http://192.168.1.31:49152/desc.xml", "")
            .unwrap();
        let found = |q: &str| app.find_listed_speaker(q).map(|m| (m.id, m.how));
        assert_eq!(found("kitchen"), Some((kitchen.clone(), PlayerMatchKind::FullName)));
        assert_eq!(found("KITCH"), Some((kitchen.clone(), PlayerMatchKind::PartOfName)));
        assert_eq!(found("192.168.1.30"), Some((kitchen.clone(), PlayerMatchKind::Address)));
        assert_eq!(found("192.168.1.31"), Some((den.clone(), PlayerMatchKind::Address)));
        assert_eq!(found("Bathroom"), None);
        assert_eq!(found("192.168.1.3"), None, "an address matches whole");
        // The list's label isn't a name: "(AirPlay, added manually)" isn't searched.
        assert_eq!(found("manually"), None);
        // A hidden speaker is reached by its full name, not by part of it.
        let mut hidden = app.speaker_settings(&den);
        hidden.hidden = true;
        app.set_speaker_settings(&den, hidden);
        assert_eq!(found("den"), Some((den.clone(), PlayerMatchKind::FullName)));
        assert_eq!(found("de"), None);
    }

    /// Wait for `cond`, polling; false after `PATIENCE`.
    fn eventually(cond: impl Fn() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        cond()
    }

    #[test]
    fn a_player_wish_waits_for_its_speaker_and_then_connects() {
        let app = test_app();
        app.connect_when_listed_every("Den".into(), Duration::from_millis(5));
        assert_eq!(app.player_wish(), Some(PlayerWish::Looking { query: "Den".into() }));
        // Nothing by that name yet: no attempt, still waiting.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(app.current_error(), None);
        // It appears (nothing answers there, so the connect fails at once).
        app.add_manual_speaker(ManualKind::Upnp, "Den", "http://127.0.0.1:9/desc.xml", "").unwrap();
        assert!(
            eventually(|| app.current_error().is_some_and(|e| e.starts_with("Couldn't connect"))),
            "no connect attempt: {:?}",
            app.current_error()
        );
        // The window says it failed, and why, while it waits for it to come back.
        assert!(
            eventually(|| matches!(app.player_wish(), Some(PlayerWish::Failed { .. }))),
            "{:?}",
            app.player_wish()
        );
        match app.player_wish() {
            Some(PlayerWish::Failed { speaker, reason, needs_person, .. }) => {
                assert_eq!(speaker, "Den");
                let reason = reason.expect("the connect's reason");
                assert!(reason.contains("127.0.0.1:9") && !reason.starts_with("Couldn't connect"), "{reason}");
                assert!(!needs_person);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_player_wish_with_the_engine_retries_only_when_the_speaker_returns() {
        let app = test_app();
        let windows = crate::platform::fake::FakePlatform::new();
        let engine = crate::engine::Engine::start(app.clone(), windows.platform()).unwrap();
        let add = || app.add_manual_speaker(ManualKind::Upnp, "Den", "http://127.0.0.1:9/den.xml", "").unwrap();
        let id = add();
        app.connect_when_listed_every("Den".into(), Duration::from_millis(5));
        let failed = || matches!(app.player_wish(), Some(PlayerWish::Failed { reason: Some(_), .. }));
        assert!(eventually(failed), "{:?}", app.player_wish());
        app.remove_manual_speaker(&id);
        assert!(eventually(|| matches!(app.player_wish(), Some(PlayerWish::Looking { .. }))));
        add();
        assert!(eventually(failed));
        use crate::model::{Command, Port, Surface};
        engine.send(Surface::Panel, Command::StayOnThisPc).wait(Duration::from_secs(5)).unwrap();
        assert!(eventually(|| app.player_wish().is_none()));
        engine.quit();
        app.request_shutdown();
    }

    #[test]
    fn a_player_wish_gives_way_to_the_users_pick() {
        let app = test_app();
        app.connect_when_listed_every("Den".into(), Duration::from_millis(5));
        app.note_user_pick();
        assert!(eventually(|| app.player_wish().is_none()), "still waiting after the user picked");
        assert_eq!(player_wait_ended("Den"), "--player \"Den\": where the sound plays changed meanwhile; no longer waiting");
    }

    #[test]
    fn this_pcs_address_is_the_first_one_known() {
        let v4 = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        let any = ip("0.0.0.0");
        let never = || -> Option<IpAddr> { panic!("looked up after an answer") };
        let never4 = || -> Option<Ipv4Addr> { panic!("looked up after an answer") };
        assert_eq!(this_pc_address_from(Some(ip("10.9.9.9")), ip("10.0.0.2"), Some(v4("10.0.0.3")), never, never, never4), ip("10.9.9.9"));
        assert_eq!(this_pc_address_from(None, ip("10.0.0.2"), Some(v4("10.0.0.3")), never, never, never4), ip("10.0.0.2"));
        assert_eq!(this_pc_address_from(None, any, Some(v4("10.0.0.3")), never, never, never4), ip("10.0.0.3"));
        assert_eq!(this_pc_address_from(None, any, None, || Some(ip("10.0.0.4")), never, never4), ip("10.0.0.4"));
        assert_eq!(this_pc_address_from(None, any, None, || None, || Some(ip("10.0.0.5")), never4), ip("10.0.0.5"));
        assert_eq!(this_pc_address_from(None, any, None, || None, || None, || Some(v4("10.0.0.6"))), ip("10.0.0.6"));
        // No network at all: still an address, this PC's own.
        assert_eq!(this_pc_address_from(None, any, None, || None, || None, || None), ip("127.0.0.1"));
    }

    #[test]
    fn the_web_link_names_the_one_address_the_server_listens_on() {
        let app = app_without_advertise_ip("127.0.0.1:8080");
        assert_eq!(link_host(&app), "127.0.0.1:8080");
    }

    #[test]
    fn the_web_link_follows_the_adapter_chosen_for_discovery() {
        let app = app_without_advertise_ip("0.0.0.0:8080");
        *app.discovery_scope.lock().unwrap() =
            Resolved::Active { name: "Wi-Fi".into(), ip: "10.1.2.3".parse().unwrap() };
        assert_eq!(link_host(&app), "10.1.2.3:8080");
        // The adapter is down: the link names something else.
        *app.discovery_scope.lock().unwrap() = Resolved::Paused { name: "Wi-Fi".into() };
        assert_ne!(link_host(&app), "10.1.2.3:8080");
    }

    #[test]
    fn the_web_link_names_this_pc_on_the_speakers_network() {
        let app = app_without_advertise_ip("0.0.0.0:8080");
        // The speaker is on this PC: Windows reaches it from loopback.
        bind(&app, upnp_session(""));
        assert_eq!(link_host(&app), "127.0.0.1:8080");
    }

    #[test]
    fn turning_the_web_ui_on_makes_its_secret_once_and_keeps_it() {
        let app = test_app();
        assert!(!app.is_web_ui_enabled());
        assert_eq!(app.web_link(), None);
        app.set_web_ui_enabled(true).unwrap();
        let stored = app.user_config.lock().unwrap().web_secret.clone().expect("stored");
        assert!(WebSecret::parse(&stored).is_some(), "{stored}");
        let link = app.web_link().expect("a link while on");
        assert_eq!(link, format!("http://192.168.1.10:8080/r/{stored}/"));
        assert!(app.web_access.path_key_ok(&stored));
        // Off and on again: the same link, so a phone's bookmark still works.
        app.set_web_ui_enabled(false).unwrap();
        assert_eq!(app.web_link(), None);
        assert!(!app.web_access.is_enabled());
        app.set_web_ui_enabled(true).unwrap();
        assert_eq!(app.web_link().as_deref(), Some(link.as_str()));
        // A new link: a new key, stored, and the old one refused at once.
        app.new_web_link().unwrap();
        let newer = app.user_config.lock().unwrap().web_secret.clone().unwrap();
        assert_ne!(newer, stored);
        assert!(!app.web_access.path_key_ok(&stored));
        assert!(app.web_access.path_key_ok(&newer));
        assert_eq!(app.web_link(), Some(format!("http://192.168.1.10:8080/r/{newer}/")));
        // Nothing about it reached a message the window shows.
        assert_eq!(app.current_error(), None);
    }

    #[test]
    fn a_new_link_that_cant_be_saved_leaves_the_current_one_in_force() {
        // Settings that can't be saved (config.json couldn't be read at
        // startup): a key kept only in memory would be undone by the next
        // start, which brings back the link the user meant to retire.
        let app = test_app();
        let key = "00112233445566778899aabbccddeeff";
        let mut settings = UserConfig::unsaveable();
        settings.web_secret = Some(key.into());
        *app.user_config.lock().unwrap() = settings;
        app.set_web_ui_enabled(true).unwrap();
        let link = app.web_link().expect("a link while on");
        let err = app.new_web_link().unwrap_err();
        // Nothing changed: the same link, its key still let in, and the
        // settings still hold it, so a later save can't store a key the
        // window never showed.
        assert_eq!(app.web_link().as_deref(), Some(link.as_str()));
        assert!(app.web_access.path_key_ok(key));
        assert_eq!(app.user_config.lock().unwrap().web_secret.as_deref(), Some(key));
        // The window shows this after "Couldn't make a new link: ". It says
        // the old link still works, and names no key.
        assert!(err.contains("couldn't be saved") && err.ends_with("The current link still works."), "{err}");
        assert!(!err.as_bytes().windows(32).any(|w| w.iter().all(u8::is_ascii_hexdigit)), "{err}");
    }

    #[test]
    fn a_stored_web_secret_is_used_and_an_unusable_one_replaced() {
        let app = test_app();
        let key = "00112233445566778899aabbccddeeff";
        app.user_config.lock().unwrap().web_secret = Some(key.into());
        app.set_web_ui_enabled(true).unwrap();
        assert_eq!(app.web_link(), Some(format!("http://192.168.1.10:8080/r/{key}/")));
        for bad in ["", "short", "zz112233445566778899aabbccddeeff"] {
            let app = test_app();
            app.user_config.lock().unwrap().web_secret = Some(bad.into());
            app.set_web_ui_enabled(true).unwrap();
            let stored = app.user_config.lock().unwrap().web_secret.clone().unwrap();
            assert!(WebSecret::parse(&stored).is_some(), "{bad:?} became {stored:?}");
            assert!(app.web_access.path_key_ok(&stored));
        }
    }

    #[test]
    fn the_web_ui_takes_the_key_web_link_stored_while_the_app_ran() {
        // The app runs with its web UI off and no key. `--web-link`, a
        // process of its own, stores one and prints the link; the app saves
        // a setting; then the web UI is turned on in the window.
        let dir = std::env::temp_dir().join(format!("sts-app-web-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let app = test_app();
        *app.user_config.lock().unwrap() = UserConfig::at_path(path.clone());
        let printed = web_access::link_from_settings(
            "192.168.1.10",
            8080,
            || UserConfig::load_from(&path),
            |c| c.save_to(&path),
        )
        .unwrap();
        app.dismiss_onboarding();
        app.set_web_ui_enabled(true).unwrap();
        assert_eq!(app.web_link().as_deref(), Some(printed.as_str()));
        // The file holds that key and the app's setting.
        let disk = UserConfig::load_from(&path);
        assert_eq!(disk.web_secret, app.web_access.secret_hex());
        assert!(disk.onboarding_dismissed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whatever a receiver answers to pair-verify, the outcome only says
    /// what to tell the user; nothing in it can delete the stored pairing.
    #[test]
    fn ap2_failures_map_to_their_pin_reasons() {
        let addr = ip("192.168.1.30");
        assert!(matches!(
            ap2_attempt_error(Ap2StartError::NeedsPin, addr),
            AttemptError::NeedsPin(PinReason::Required)
        ));
        match ap2_attempt_error(Ap2StartError::PairingNotProven(anyhow!("pair-verify M1 → 400 Bad Request")), addr) {
            AttemptError::NeedsPin(PinReason::NotProven { addr: a, detail }) => {
                assert_eq!(a, addr);
                assert!(detail.contains("400"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            ap2_attempt_error(Ap2StartError::PairingRefused(anyhow!("M4 error 2")), addr),
            AttemptError::NeedsPin(PinReason::Refused)
        ));
        assert!(matches!(
            ap2_attempt_error(Ap2StartError::Other(anyhow!("timed out")), addr),
            AttemptError::Other(m) if m.contains("timed out")
        ));
    }

    /// A receiver's answer can run to a MiB; the window shows its start.
    #[test]
    fn a_long_receiver_answer_is_cut_short_in_the_window() {
        let body = "é".repeat(1 << 20);
        let e = anyhow!("pair-verify M1 → 400 Bad Request (body: {body})");
        let reason = match ap2_attempt_error(Ap2StartError::PairingNotProven(e), ip("192.168.1.30")) {
            AttemptError::NeedsPin(r @ PinReason::NotProven { .. }) => r,
            other => panic!("{other:?}"),
        };
        let PinReason::NotProven { detail, .. } = &reason else { unreachable!() };
        assert_eq!(detail.chars().count(), RECEIVER_DETAIL_CHARS + 1);
        assert!(
            detail.starts_with("pair-verify M1 → 400 Bad Request (body: é") && detail.ends_with("é…"),
            "{detail}"
        );
        let shown = pin_pairing_message("Den TV", &reason, true);
        assert!(shown.chars().count() < 3 * RECEIVER_DETAIL_CHARS, "{} chars", shown.chars().count());
        assert_eq!(
            clip_chars("pair-verify M1 → 400 Bad Request", RECEIVER_DETAIL_CHARS),
            "pair-verify M1 → 400 Bad Request"
        );
        assert_eq!(clip_chars("abc", 3), "abc");
        assert_eq!(clip_chars("abcd", 3), "abc…");
    }

    /// Each reason reads differently, and says what happens next: the PIN
    /// on screen when the ceremony is starting, where to start it when it
    /// can't (web API, tray, reconnects).
    #[test]
    fn pin_messages_say_why_and_what_next() {
        let not_proven = PinReason::NotProven {
            addr: ip("192.168.1.30"),
            detail: "pair-verify M1 → 400 Bad Request".into(),
        };
        let damaged = PinReason::Damaged("controller seed: bad hex at offset 4".into());
        let m = |r: &PinReason, i: bool| pin_pairing_message("Den TV", r, i);

        assert_eq!(
            m(&PinReason::Required, true),
            "Den TV needs one-time pairing — enter the PIN shown on its screen."
        );
        assert!(m(&PinReason::Required, false).contains("open the Stream To Speaker window"));

        let shown = m(&not_proven, true);
        assert!(
            shown.contains("192.168.1.30") && shown.contains("400") && shown.contains("stored pairing is kept"),
            "{shown}"
        );
        assert!(shown.contains("if not, cancel"), "{shown}");
        assert!(m(&not_proven, false).contains("If Den TV was reset, open the Stream To Speaker window"));

        let refused = m(&PinReason::Refused, true);
        assert!(
            refused.contains("no longer accepts this PC's pairing")
                && refused.ends_with("enter the PIN shown on its screen."),
            "{refused}"
        );
        assert!(m(&PinReason::Refused, false).ends_with("click it in the speaker list."));

        let broken = m(&damaged, false);
        assert!(broken.contains("damaged") && broken.contains("controller seed"), "{broken}");
    }

    /// Stored PIN pairing credentials (only parsed when connecting).
    fn some_pairing(tag: &str) -> PairingCredentials {
        PairingCredentials {
            controller_id: format!("ctl-{tag}"),
            controller_seed_hex: "11".repeat(32),
            accessory_id: format!("acc-{tag}"),
            accessory_ltpk_hex: "22".repeat(32),
        }
    }

    /// The way out of a stored pairing goes with the device that holds it:
    /// its settings and the reports of its PIN pairing failing offer to
    /// forget it whichever speaker is saved (another one, or none after
    /// Forget saved), and only while a pairing is stored.
    #[test]
    fn forgetting_a_pairing_is_offered_for_the_device_that_holds_it() {
        let app = test_app();
        let (den, kitchen) = ("airplay:DEN", "airplay:KITCHEN");
        {
            let mut uc = app.user_config.lock().unwrap();
            uc.airplay_pairings.insert(den.into(), some_pairing("den"));
            uc.last_speaker_id = Some("uuid:RINCON_000E58000001".into());
        }
        assert!(app.has_airplay_pairing(den));
        assert!(!app.has_airplay_pairing(kitchen));
        app.user_config.lock().unwrap().last_speaker_id = None;
        assert!(app.has_airplay_pairing(den));

        let hint = forget_pairing_hint("Den TV", app.has_airplay_pairing(den));
        assert!(
            hint.starts_with(" If Den TV no longer asks for device verification")
                && hint.ends_with("⚙ next to it in the speaker list → Forget pairing."),
            "{hint}"
        );
        assert_eq!(forget_pairing_hint("Kitchen", app.has_airplay_pairing(kitchen)), "");
    }

    /// Song titles go only from music apps that play, and never while
    /// another app's sound plays too: a browser's title never goes out.
    #[test]
    fn browser_titles_never_go_out() {
        let track = |t: &str| crate::now_playing::NowPlaying { title: t.into(), artist: "A".into(), album: "B".into() };
        let source = |app: Option<&str>, playing, t: &str| TitleSource::from_report(app.map(Into::into), playing, Some(track(t)));
        let spotify = source(Some("Spotify.exe"), Some(true), "Song");
        assert_eq!(title_to_send(&[spotify.clone()], &[]), Some(track("Song")));
        for browser in ["chrome.exe", "msedge.exe", "firefox.exe", "C:\\Program Files\\Opera\\opera.exe", "brave"] {
            assert_eq!(title_to_send(&[source(Some(browser), Some(true), "A video")], &[]), None, "{browser}");
        }
        for video in ["vlc.exe", "Microsoft.ZuneVideo", "mpc-hc64.exe", "Netflix"] {
            assert_eq!(title_to_send(&[source(Some(video), Some(true), "A film")], &[]), None, "{video}");
        }
        // An app Windows doesn't name: nothing.
        assert_eq!(title_to_send(&[source(None, Some(true), "Song")], &[]), None);
        for status in [Some(false), None] {
            assert_eq!(title_to_send(&[source(Some("Spotify.exe"), status, "Song")], &[]), None);
        }
        // A browser first and Spotify playing: Spotify's.
        assert_eq!(
            title_to_send(&[source(Some("chrome.exe"), Some(true), "A video"), spotify], &[]),
            Some(track("Song"))
        );
        assert!(is_music_app("AppleInc.AppleMusicWin_nzyj5cx40ttqa!App"));
        // A Store app's media session and its own program's sound are one
        // app: Apple Music's title goes out while only Apple Music sounds.
        let apple = source(Some("AppleInc.AppleMusicWin_nzyj5cx40ttqa!App"), Some(true), "Song");
        assert_eq!(title_to_send(&[apple.clone()], &["applemusic".into()]), Some(track("Song")));
        assert_eq!(title_to_send(&[apple], &["applemusic".into(), "tidal".into()]), None);
        assert!(is_music_app("C:\\Users\\x\\AppData\\Roaming\\Spotify\\Spotify.exe"));
        assert!(!is_music_app(""));
    }

    /// An AirPlay speaker, whose level can't be read, starts at the level
    /// last set from this PC within three hours, later at no more than 30;
    /// the first time at 25; never above its maximum from this PC.
    #[test]
    fn airplay_starts_at_most_at_30_after_three_hours() {
        let now = 1_800_000_000;
        let hours = |h: u64| now - h * 3600;
        assert_eq!(start_level(Some(70), Some(hours(1)), now, None), 70);
        assert_eq!(start_level(Some(70), Some(hours(3)), now, None), 70);
        assert_eq!(start_level(Some(70), Some(hours(3) - 1), now, None), LEVEL_LATER_AT_MOST);
        assert_eq!(start_level(Some(20), Some(hours(30)), now, None), 20, "the lower of the two");
        assert_eq!(start_level(Some(70), None, now, None), LEVEL_LATER_AT_MOST, "a level of unknown age is old");
        assert_eq!(start_level(None, None, now, None), crate::engine::place::numbers::LEVEL_FIRST);
        assert_eq!(start_level(Some(70), Some(hours(1)), now, Some(40)), 40);
        assert_eq!(start_level(None, None, now, Some(10)), 10);
        // From the stored settings.
        let app = test_app();
        let mut s = app.speaker_settings("airplay:AA");
        s.level = Some(64);
        s.level_at = Some(unix_seconds() - 600);
        app.set_speaker_settings("airplay:AA", s.clone());
        assert_eq!(app.airplay_start_volume("airplay:AA"), 64);
        s.level_at = Some(unix_seconds() - 4 * 3600);
        app.set_speaker_settings("airplay:AA", s.clone());
        assert_eq!(app.airplay_start_volume("airplay:AA"), 30);
        s.volume_limit = Some(20);
        app.set_speaker_settings("airplay:AA", s);
        assert_eq!(app.airplay_start_volume("airplay:AA"), 20);
    }

    /// An app with AirPlay discovery, on in-memory settings.
    fn airplay_test_app() -> (Arc<App>, Arc<AirPlayDiscoveryState>) {
        airplay_test_app_with(AirPlayDiscoveryState::new())
    }

    fn airplay_test_app_with(discovery: Arc<AirPlayDiscoveryState>) -> (Arc<App>, Arc<AirPlayDiscoveryState>) {
        // Never the settings of whoever runs the tests; no `--advertise-ip`,
        // so each receiver gets this PC's address toward it.
        let app = App::with_settings(
            AppConfig { advertise_ip: None, ..App::test_config() },
            UserConfig::in_memory(),
            None,
            Some(discovery.clone()),
        );
        (app, discovery)
    }

    const KITCHEN: &str = "airplay:B8E93735AC11";
    const KITCHEN_2: &str = "airplay:B8E93735AC11/b8e93735ac11@kitchen 2._raop._tcp.local.";

    /// Discovery on 192.168.1.0/24 that has a speaker (Kitchen, with an
    /// AirPlay 2 record) and a newcomer claiming its MAC at the same
    /// address and one more.
    fn conflict_discovery() -> Arc<AirPlayDiscoveryState> {
        use mdns_sd::ServiceInfo;
        let d = AirPlayDiscoveryState::with_lan(crate::discovery_net::LanTargets::from_networks([("192.168.1.10".parse().unwrap(), 24)]));
        let raop = crate::airplay::discovery::RAOP_SERVICE;
        let txt = &[("et", "0,1"), ("cn", "0,1")][..];
        d.ingest_for_tests(raop, &ServiceInfo::new(raop, "B8E93735AC11@Kitchen", "k.local.", "192.168.1.65", 5000, txt).unwrap());
        d.ingest_for_tests(raop, &ServiceInfo::new(raop, "B8E93735AC11@Kitchen 2", "e.local.", "192.168.1.65,192.168.1.66", 6666, txt).unwrap());
        let ap2 = crate::airplay::discovery::AIRPLAY_SERVICE;
        let ap2_txt = &[("deviceid", "B8:E9:37:35:AC:11"), ("features", "0x445F8A00,0x1C340")][..];
        d.ingest_for_tests(ap2, &ServiceInfo::new(ap2, "Kitchen", "k.local.", "192.168.1.65", 7000, ap2_txt).unwrap());
        Arc::new(d)
    }

    /// A newcomer is sent nothing stored for the speaker: not under its own
    /// id, and not when its record carries the speaker's plain id (the
    /// gate asks the record). The speaker itself is.
    #[test]
    fn a_newcomer_never_gets_the_stored_password() {
        use crate::airplay::fake_raop::{Answer, FakeRaop, Reply};
        let rx = FakeRaop::start(|req| match req.header("Authorization") {
            None => Answer::Reply(Reply::status(401, "Unauthorized").header("WWW-Authenticate", "Digest realm=\"raop\", nonce=\"1\"")),
            Some(_) => Answer::Normal,
        });
        let (app, _discovery) = airplay_test_app();
        let mut incumbent = crate::airplay::raop_transcripts::receiver(&rx, "0D0000000001", &[0, 1]);
        incumbent.raop.instance = Some("0d0000000001@speaker._raop._tcp.local.".into());
        app.user_config.lock().unwrap().airplay_passwords.insert(incumbent.stable_id(), "hunter2".into());
        let mut newcomer = incumbent.clone();
        newcomer.raop.instance = Some("0d0000000001@speaker 2._raop._tcp.local.".into());
        newcomer.raop.newcomer = true;
        let mut newcomer_plain_id = incumbent.clone();
        newcomer_plain_id.raop.instance = None;
        newcomer_plain_id.raop.newcomer = true;
        assert_eq!(newcomer_plain_id.stable_id(), incumbent.stable_id());

        for n in [newcomer, newcomer_plain_id] {
            assert!(app.start_airplay_one(Transport::RaopLegacy, n, ip("127.0.0.1"), &app.hub).is_err());
        }
        assert!(rx.requests().iter().all(|r| r.header("Authorization").is_none()), "{:?}", rx.lines());

        match app.start_airplay_one(Transport::RaopLegacy, incumbent, ip("127.0.0.1"), &app.hub) {
            Ok(s) => s.stop(),
            Err(_) => panic!("the incumbent connects with its password"),
        }
        assert!(rx.requests().iter().any(|r| r.header("Authorization").is_some()));
    }

    /// Nothing is stored under a newcomer's id (`airplay:<MAC>/<instance>`),
    /// also once its conflict has ended and the record left is no longer
    /// marked: such an id only ever names a newcomer, and nothing would
    /// read or remove what was stored under it.
    #[test]
    fn nothing_is_stored_under_a_newcomers_id() {
        let (app, discovery) = airplay_test_app_with(conflict_discovery());
        discovery.remove_for_tests(crate::airplay::discovery::RAOP_SERVICE, "B8E93735AC11@Kitchen._raop._tcp.local.");
        assert!(!discovery.find_by_id(KITCHEN_2).unwrap().is_newcomer(), "the conflict has ended");
        app.set_airplay_password(KITCHEN_2, "typed-after-conflict");
        app.set_speaker_settings(KITCHEN_2, SpeakerSettings { hidden: true, ..Default::default() });
        app.store_airplay_pairing(KITCHEN_2, some_pairing("kitchen-2"));
        let uc = app.user_config.lock().unwrap();
        assert!(uc.airplay_passwords.is_empty() && uc.speaker_settings.is_empty() && uc.airplay_pairings.is_empty());
        let written = serde_json::to_string(&*uc).unwrap();
        assert!(!written.contains("typed-after-conflict") && !written.contains("kitchen 2"), "{written}");
    }

    /// Whether a typed password is kept or used for one connection is
    /// decided when the dialog opens. When the conflict ends while it is
    /// open (the speaker's own announcement goes, the instance left is no
    /// longer marked), what is typed for the second claimant is still
    /// used for that one connection, and is not in the settings or in
    /// what a save would write.
    #[test]
    fn a_password_typed_as_the_conflict_ends_is_for_that_connection_only() {
        let (app, discovery) = airplay_test_app_with(conflict_discovery());
        app.user_config.lock().unwrap().airplay_passwords.insert(KITCHEN.into(), "hunter2".into());
        let dialog = app.password_dialog(KITCHEN_2, "Kitchen 2");
        assert!(dialog.one_session);
        discovery.remove_for_tests(crate::airplay::discovery::RAOP_SERVICE, "B8E93735AC11@Kitchen._raop._tcp.local.");
        let left = discovery.find_by_id(KITCHEN_2).unwrap();
        assert!(!left.is_newcomer());
        app.enter_airplay_password(&dialog, "typed-after-conflict").unwrap();
        {
            let uc = app.user_config.lock().unwrap();
            assert_eq!(uc.airplay_passwords.values().collect::<Vec<_>>(), ["hunter2"]);
            assert!(!serde_json::to_string(&*uc).unwrap().contains("typed-after-conflict"));
        }
        // The connect the dialog starts uses it, and only that one.
        assert_eq!(app.airplay_password_for(&left), (Some("typed-after-conflict".into()), true));
        assert_eq!(app.airplay_password_for(&left), (Some("hunter2".into()), false));
    }

    /// When the entry the dialog was opened for has gone by the time the
    /// person confirms, nothing is stored or kept, nothing is connected,
    /// and they are told what to do.
    #[test]
    fn a_password_for_an_entry_that_has_gone_is_not_kept() {
        use mdns_sd::ServiceInfo;
        let (app, discovery) = airplay_test_app_with(conflict_discovery());
        let raop = crate::airplay::discovery::RAOP_SERVICE;
        let txt = &[("et", "0,1"), ("cn", "0,1"), ("pw", "true")][..];
        discovery.ingest_for_tests(raop, &ServiceInfo::new(raop, "0D0000000009@Den", "d.local.", "192.168.1.70", 5000, txt).unwrap());
        let dialogs = [app.password_dialog(KITCHEN_2, "Kitchen 2"), app.password_dialog("airplay:0D0000000009", "Den")];
        assert_eq!(dialogs.iter().map(|d| d.one_session).collect::<Vec<_>>(), [true, false]);
        discovery.remove_for_tests(raop, "B8E93735AC11@Kitchen 2._raop._tcp.local.");
        discovery.remove_for_tests(raop, "0D0000000009@Den._raop._tcp.local.");
        for d in &dialogs {
            let err = app.submit_password_dialog(d, "typed-for-a-gone-one").expect_err("not taken");
            assert_eq!(
                err,
                format!(
                    "Couldn't connect: “{}” is no longer in the speaker list, and the password you typed wasn't kept. \
                     When it's back in the list, pick it and enter the password again.",
                    d.name
                )
            );
        }
        assert!(app.connecting_to().is_none());
        assert!(app.typed_airplay_passwords.lock().unwrap().is_empty());
        let uc = app.user_config.lock().unwrap();
        assert!(uc.airplay_passwords.is_empty());
        assert!(!serde_json::to_string(&*uc).unwrap().contains("typed-for-a-gone-one"));
    }

    /// A password typed for one connection doesn't outlive the connect it
    /// was typed for, even one that never got as far as using it.
    #[test]
    fn a_typed_password_is_forgotten_when_its_connect_ends() {
        let (app, discovery) = airplay_test_app_with(conflict_discovery());
        let dialog = app.password_dialog(KITCHEN_2, "Kitchen 2");
        app.enter_airplay_password(&dialog, "typed").unwrap();
        // Gone before the connect looks it up, so nothing is contacted.
        discovery.remove_for_tests(crate::airplay::discovery::RAOP_SERVICE, "B8E93735AC11@Kitchen 2._raop._tcp.local.");
        app.select_speaker_async(KITCHEN_2);
        let until = Instant::now() + Duration::from_secs(10);
        while app.connecting_to().is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.connecting_to().is_none());
        assert!(app.typed_airplay_passwords.lock().unwrap().is_empty());
    }

    /// A password typed for a newcomer is used for its next connect and
    /// not stored; the speaker's stored password stays the speaker's.
    #[test]
    fn a_password_typed_for_a_newcomer_is_used_once_and_not_stored() {
        use crate::airplay::fake_raop::{Answer, FakeRaop, Reply};
        let rx = FakeRaop::start(|req| match req.header("Authorization") {
            None => Answer::Reply(Reply::status(401, "Unauthorized").header("WWW-Authenticate", "Digest realm=\"raop\", nonce=\"1\"")),
            Some(_) => Answer::Normal,
        });
        let (app, discovery) = airplay_test_app_with(conflict_discovery());
        assert!(discovery.find_by_id(KITCHEN_2).unwrap().is_newcomer());
        app.user_config.lock().unwrap().airplay_passwords.insert(KITCHEN.into(), "hunter2".into());

        // The window's password dialog stores what was typed, then connects.
        app.set_airplay_password(KITCHEN_2, "typed");
        assert!(!app.user_config.lock().unwrap().airplay_passwords.contains_key(KITCHEN_2));

        // The newcomer's record, pointed at the test receiver.
        let mut newcomer = crate::airplay::raop_transcripts::receiver(&rx, "B8E93735AC11", &[0, 1]);
        newcomer.raop.instance = Some("b8e93735ac11@kitchen 2._raop._tcp.local.".into());
        newcomer.raop.newcomer = true;
        assert_eq!(newcomer.stable_id(), KITCHEN_2);
        match app.start_airplay_one(Transport::RaopLegacy, newcomer.clone(), ip("127.0.0.1"), &app.hub) {
            Ok(s) => s.stop(),
            Err(_) => panic!("the typed password is used"),
        }
        let auth: Vec<_> = rx.requests().iter().filter_map(|r| r.header("Authorization").map(str::to_string)).collect();
        assert!(!auth.is_empty());
        // Only for that connect.
        let before = rx.requests().len();
        assert!(app.start_airplay_one(Transport::RaopLegacy, newcomer, ip("127.0.0.1"), &app.hub).is_err());
        assert!(rx.requests()[before..].iter().all(|r| r.header("Authorization").is_none()));
        assert_eq!(app.user_config.lock().unwrap().airplay_passwords.len(), 1);
    }

    /// A speaker that asks for a password it never announced gets the
    /// password dialog on the next pick, as one that announces it does.
    #[test]
    fn a_speaker_that_asks_for_a_password_gets_the_dialog_next_time() {
        use crate::airplay::fake_raop::{Answer, FakeRaop, Reply};
        let rx = FakeRaop::start(|_| {
            Answer::Reply(Reply::status(401, "Unauthorized").header("WWW-Authenticate", "Digest realm=\"raop\", nonce=\"1\""))
        });
        let (app, _discovery) = airplay_test_app();
        let r = crate::airplay::raop_transcripts::receiver(&rx, "0D0000000005", &[0, 1]);
        let id = r.stable_id();
        assert!(!app.is_password_protected(&id));
        let err = match app.start_airplay_one(Transport::RaopLegacy, r, ip("127.0.0.1"), &app.hub) {
            Err(AttemptError::Other(e)) => e,
            _ => panic!("refused"),
        };
        assert!(err.starts_with("Test Speaker asks for its AirPlay password."), "{err}");
        assert!(app.is_password_protected(&id));
    }

    /// A receiver with an AirPlay 2 path whose AirPlay 1 record the app
    /// always took for unsupported (`et` with 4 but not 0 or 1, no `cn`,
    /// no `et`) is still played over AirPlay 2 only, as before: the
    /// lenient readings of those records are for receivers without
    /// AirPlay 2.
    #[test]
    fn airplay_2_receivers_keep_their_routing() {
        let (app, _discovery) = airplay_test_app();
        let ap2 = |et: Vec<u8>, cn: Vec<u8>| {
            let mut r = crate::airplay::raop_transcripts::receiver_at(ip("192.168.1.5"), 5000, "0D0000000010", &et);
            r.codecs = cn;
            r.features = Some(crate::airplay::discovery::FEAT_AUDIO | crate::airplay::discovery::FEAT_TRANSIENT_PAIRING);
            r.airplay_port = Some(7000);
            r
        };
        for (et, cn) in [(vec![4], vec![0, 1]), (vec![4, 5], vec![0, 1]), (vec![0, 4], vec![]), (vec![], vec![0, 1])] {
            let r = ap2(et.clone(), cn.clone());
            let attempts: Vec<Transport> = app.airplay_attempts(&r, None).into_iter().map(|(t, _)| t).collect();
            assert_eq!(attempts, [Transport::AirPlay2], "et {et:?} cn {cn:?}");
        }
        // A record the app has always played keeps its AirPlay 1 attempt
        // first.
        let attempts: Vec<Transport> = app.airplay_attempts(&ap2(vec![0, 4], vec![0, 1]), None).into_iter().map(|(t, _)| t).collect();
        assert_eq!(attempts, [Transport::RaopLegacy, Transport::AirPlay2]);
    }

    /// Nothing automatic connects to a newcomer (the drop reconnect and
    /// "connect always" come through here without a person's choice).
    #[test]
    fn nothing_automatic_connects_to_a_newcomer() {
        let (app, _discovery) = airplay_test_app_with(conflict_discovery());
        let err = app.select_speaker_opts(KITCHEN_2, false, false).expect_err("refused");
        assert!(err.starts_with("Not connecting to ") && err.contains("same identity as another speaker"), "{err}");
    }

    /// The web API (and the other callers of `select_speaker`: the launch
    /// connect, a resync, Enable) refuses a newcomer; the window's pick is
    /// the only way to play to it.
    #[test]
    fn the_web_api_refuses_a_newcomer() {
        let (app, _discovery) = airplay_test_app_with(conflict_discovery());
        let err = app.select_speaker(KITCHEN_2).expect_err("refused");
        assert_eq!(
            err,
            "To play to “Kitchen — another receiver with the same ID: Kitchen 2 (192.168.1.66)”, pick it in the Stream To \
             Speaker window."
        );
        assert!(app.connecting_to().is_none());
    }

    /// `--player` never takes a newcomer, whatever it is asked for: not by
    /// its address, its own name or its row's name, and not in place of
    /// the speaker when the speaker is hidden. The speaker is found as
    /// usual.
    #[test]
    fn player_never_takes_a_newcomer() {
        use mdns_sd::ServiceInfo;
        let (app, discovery) = airplay_test_app_with(conflict_discovery());
        // One more, at nothing but the speaker's own address.
        let raop = crate::airplay::discovery::RAOP_SERVICE;
        let txt = &[("et", "0,1"), ("cn", "0,1")][..];
        discovery.ingest_for_tests(raop, &ServiceInfo::new(raop, "B8E93735AC11@Kitchen 3", "e3.local.", "192.168.1.65", 6667, txt).unwrap());
        let kitchen_3 = "airplay:B8E93735AC11/b8e93735ac11@kitchen 3._raop._tcp.local.";
        assert!(discovery.find_by_id(kitchen_3).unwrap().is_newcomer());
        // While the two claims last, each row's name carries its address.
        let speaker_row = discovery.find_by_id(KITCHEN).unwrap().friendly_name;
        let newcomer_row = discovery.find_by_id(KITCHEN_2).unwrap().friendly_name;
        let found = |q: &str| app.find_listed_speaker(q).map(|m| (m.id, m.how));

        assert_eq!(found(&speaker_row), Some((KITCHEN.to_string(), PlayerMatchKind::FullName)));
        assert_eq!(found("192.168.1.65"), Some((KITCHEN.to_string(), PlayerMatchKind::Address)));
        assert_eq!(found("Kit").map(|(id, _)| id), Some(KITCHEN.to_string()));
        for q in ["Kitchen 2", "Kitchen 3", "192.168.1.66", newcomer_row.as_str(), "another receiver"] {
            assert_eq!(found(q), None, "{q}");
        }
        // The speaker hidden: still reached by its address and full name,
        // and part of a name finds nothing rather than a newcomer.
        let mut hidden = app.speaker_settings(KITCHEN);
        hidden.hidden = true;
        app.set_speaker_settings(KITCHEN, hidden);
        assert_eq!(found("192.168.1.65"), Some((KITCHEN.to_string(), PlayerMatchKind::Address)));
        assert_eq!(found(&speaker_row), Some((KITCHEN.to_string(), PlayerMatchKind::FullName)));
        assert_eq!(found("Kit"), None);
    }

    /// A newcomer at the speaker's own address is never given the
    /// speaker's AirPlay 2 record as its sibling (that path would use the
    /// speaker's stored pairing): it is played over what it announced.
    #[test]
    fn a_newcomer_borrows_no_sibling_record() {
        use mdns_sd::ServiceInfo;
        let (app, discovery) = airplay_test_app_with(conflict_discovery());
        // One that announces nothing but the speaker's own address.
        let raop = crate::airplay::discovery::RAOP_SERVICE;
        let txt = &[("et", "0,1"), ("cn", "0,1")][..];
        discovery.ingest_for_tests(raop, &ServiceInfo::new(raop, "B8E93735AC11@Kitchen 3", "e3.local.", "192.168.1.65", 6667, txt).unwrap());
        let id = "airplay:B8E93735AC11/b8e93735ac11@kitchen 3._raop._tcp.local.";
        let newcomer = discovery.find_by_id(id).unwrap();
        assert_eq!(newcomer.ip, ip("192.168.1.65"));
        let attempts = app.airplay_attempts(&newcomer, Some(&discovery));
        assert_eq!(attempts.len(), 1);
        assert_eq!((attempts[0].0, attempts[0].1.stable_id().as_str()), (Transport::RaopLegacy, id));
        // The speaker itself still gets both paths.
        let kitchen = discovery.find_by_id(KITCHEN).unwrap();
        assert_eq!(app.airplay_attempts(&kitchen, Some(&discovery)).len(), 2);
    }

    /// The address that carried a session is remembered only once the
    /// session is up, not when a connect got part of the way.
    #[test]
    fn only_a_session_that_came_up_marks_its_address() {
        use crate::airplay::fake_raop::{Answer, FakeRaop, Reply};
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("RECORD", 0) => Answer::Reply(Reply::status(500, "Internal Server Error")),
            _ => Answer::Normal,
        });
        let (app, discovery) = airplay_test_app();
        let r = crate::airplay::raop_transcripts::receiver(&rx, "0D0000000002", &[0, 1]);
        assert!(app.start_airplay_one(Transport::RaopLegacy, r.clone(), ip("127.0.0.1"), &app.hub).is_err());
        assert_eq!(discovery.working_address("0D0000000002"), None);
        match app.start_airplay_one(Transport::RaopLegacy, r, ip("127.0.0.1"), &app.hub) {
            Ok(s) => s.stop(),
            Err(_) => panic!("the second connect works"),
        }
        assert_eq!(discovery.working_address("0D0000000002"), Some(ip("127.0.0.1")));
    }

    /// Starting the AirPlay 2 path with a stored pairing, whatever the
    /// receiver answers to pair-verify: the attempt says why it failed,
    /// the pairing stays as it was (only a new PIN pairing or the user's
    /// Forget pairing changes it) and transient pairing is never tried.
    #[test]
    fn a_failed_pair_verify_leaves_the_stored_pairing_alone() {
        use crate::airplay::test_peers::{self, Peer, M4};
        let app = test_app();
        let creds = test_peers::stored_credentials();
        for (peer, want) in [
            (Peer::StatusAtM1("400 Bad Request"), "not proven"),
            (Peer::Impostor, "not proven"),
            (Peer::OtherDevice, "not proven"),
            (Peer::Genuine(M4::Error(2)), "refused"),
            (Peer::Genuine(M4::Junk), "error"),
            (Peer::CloseAtM1, "error"),
        ] {
            let what = format!("{peer:?}");
            let (port, seen) = test_peers::spawn(peer);
            let receiver = AirPlayRenderer {
                friendly_name: "Den TV".into(),
                mac_id: "AABBCCDDEEFF".into(),
                ip: ip("127.0.0.1"),
                port: 0,
                airplay_port: Some(port),
                encryption_types: vec![],
                codecs: vec![],
                password_protected: false,
                encryption_key_required: false,
                features: None,
                pk: None,
                model: None,
                group: None,
                pair: None,
                raop: Default::default(),
            };
            let id = receiver.stable_id();
            app.user_config.lock().unwrap().airplay_pairings.insert(id.clone(), creds.clone());
            let got = match app.start_airplay_one(Transport::AirPlay2, receiver, ip("127.0.0.1"), &app.hub) {
                Ok(_) => "connected",
                Err(AttemptError::NeedsPin(PinReason::NotProven { .. })) => "not proven",
                Err(AttemptError::NeedsPin(PinReason::Refused)) => "refused",
                Err(AttemptError::NeedsPin(other)) => panic!("{what}: {other:?}"),
                Err(AttemptError::Other(_)) => "error",
            };
            assert_eq!(got, want, "{what}");
            assert_eq!(app.user_config.lock().unwrap().airplay_pairings.get(&id), Some(&creds), "{what}");
            let seen = seen.join().unwrap();
            assert!(seen.iter().any(|l| l.starts_with("POST /pair-verify")), "{what}: {seen:?}");
            assert!(!seen.iter().any(|l| l.starts_with("POST /pair-setup")), "{what}: {seen:?}");
        }
    }

    /// Forget pairing removes that device's pairing and no other.
    #[test]
    fn forget_pairing_removes_only_that_devices_pairing() {
        let app = test_app();
        {
            let mut uc = app.user_config.lock().unwrap();
            uc.airplay_pairings.insert("airplay:DEN".into(), some_pairing("den"));
            uc.airplay_pairings.insert("airplay:LOFT".into(), some_pairing("loft"));
        }
        app.forget_airplay_pairing("airplay:DEN");
        assert!(!app.has_airplay_pairing("airplay:DEN"));
        assert!(app.has_airplay_pairing("airplay:LOFT"));
    }

    #[test]
    fn settings_that_cannot_be_saved_are_said_so_until_a_save_works() {
        let app = App::with_settings(App::test_config(), UserConfig::unsaveable(), None, None);
        // From the start: the file couldn't be read, nothing will be kept.
        let (problem, notice) = app.settings_attention();
        assert!(problem.unwrap().contains("changes made now won't be kept"));
        assert_eq!(notice, None);
        // A password or a pairing made now says it won't be remembered.
        app.set_airplay_password("airplay:aa", "hunter2");
        assert!(app.current_error().unwrap().contains("password can't be saved"), "{:?}", app.current_error());
        assert!(!app.store_airplay_pairing("airplay:cc", some_pairing("c")));
        // Removing one is no news.
        app.dismiss_error();
        app.set_airplay_password("airplay:aa", "");
        assert_eq!(app.current_error(), None);
        // Settings that save: no strip, no message.
        let fine = test_app();
        fine.set_airplay_password("airplay:aa", "hunter2");
        assert_eq!(fine.settings_attention(), (None, None));
        assert_eq!(fine.current_error(), None);
        assert!(fine.store_airplay_pairing("airplay:cc", some_pairing("c")));
    }

    #[test]
    fn what_loading_the_settings_found_is_shown_until_dismissed() {
        let dir = std::env::temp_dir().join(format!("sts-app-load-notice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"last_speaker_id":"x","prefer_realtime_airplay":"true"}"#).unwrap();
        let app = App::with_settings(App::test_config(), UserConfig::load_from(&path), None, None);
        let (problem, notice) = app.settings_attention();
        assert_eq!(problem, None);
        let notice = notice.unwrap();
        assert!(notice.starts_with("Some settings couldn't be read and were reset or left out: \
             AirPlay 2 stream."), "{notice}");
        app.dismiss_settings_notice();
        assert_eq!(app.settings_attention(), (None, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_is_tried_again_while_the_window_is_open() {
        let dir = std::env::temp_dir().join(format!("sts-app-retry-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The settings folder can't be made: a file has its name.
        let folder = dir.join("StreamToSpeaker");
        std::fs::write(&folder, b"in the way").unwrap();
        let app = App::with_settings(App::test_config(), UserConfig::at_path(folder.join("config.json")), None, None);
        app.set_airplay_password("airplay:aa", "hunter2");
        assert!(app.settings_attention().0.is_some(), "the failure shows");
        let t0 = Instant::now();
        // Tried at once, on a thread of its own (the window doesn't wait).
        let retry = app.retry_settings_save_if_due(t0).expect("tried at once");
        assert_ne!(retry.thread().id(), std::thread::current().id());
        retry.join().unwrap();
        assert!(app.retry_settings_save_if_due(t0 + Duration::from_secs(1)).is_none(), "not again so soon");
        assert!(app.settings_attention().0.is_some());
        // The fault passes; the next try saves and the strip goes.
        std::fs::remove_file(&folder).unwrap();
        app.retry_settings_save_if_due(t0 + Duration::from_secs(6)).expect("tried again").join().unwrap();
        assert_eq!(app.settings_attention().0, None);
        assert_eq!(UserConfig::load_from(&folder.join("config.json")).airplay_passwords["airplay:aa"], "hunter2");
        // Nothing to try while saves work.
        assert!(app.retry_settings_save_if_due(t0 + Duration::from_secs(60)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
