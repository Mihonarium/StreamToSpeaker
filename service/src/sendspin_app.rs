//! App glue for Sendspin: Sendspin players in the speaker list (we are the
//! server), the one-time pairing prompt for players that need it, and the
//! Music Assistant source (we are a client that Music Assistant plays as
//! a Live Input).

use crossbeam_channel::{bounded, Sender};
use log::{debug, info};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::app::{ActiveSession, App};
use crate::discovery_net::MdnsController;
use crate::http_server::SpeakerInfo;
use crate::sendspin::mdns::DEFAULT_CLIENT_PORT;
use crate::sendspin::proto::PairMethod;
use crate::sendspin::server::{
    self, NowPlayingFn, PairingInput, PlayerSession, PlayerSessionConfig, SendspinDiscoveryState, StartError,
};
use crate::sendspin::source::{SourceOptions, SourceService, SourceState, PAIRING_FAILURE_LIMIT};
use crate::sendspin::store::SendspinConfig;
use crate::sendspin::{machine_name, SendspinStore};
use crate::PRODUCT_NAME;

/// How the pairing prompt for a Sendspin speaker asks for its secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairPromptKind {
    /// The speaker is showing or speaking a code right now.
    LiveCode,
    /// A code printed on the speaker / its leaflet, or its pairing token.
    StaticCodeOrToken,
    /// Only a pairing token (`SP:0…`) will do.
    TokenOnly,
}

/// What the GUI shows for a pending Sendspin pairing.
#[derive(Clone, Debug)]
pub struct PairPromptView {
    pub id: String,
    pub name: String,
    pub kind: PairPromptKind,
    /// We were paired before but the speaker forgot it.
    pub lost_credential: bool,
    /// A different identity answered at the name we paired with.
    pub identity_changed: bool,
}

pub(crate) struct PairPrompt {
    view: PairPromptView,
    /// Live-code attempts: the connect thread waits on the other end.
    tx: Option<Sender<Option<String>>>,
}

#[derive(Default)]
pub struct SendspinAppState {
    /// Sendspin speakers found; `Some` while Sendspin discovery is on.
    pub(crate) discovery: Mutex<Option<Arc<SendspinDiscoveryState>>>,
    /// The mDNS browse feeding `discovery`, on the discovery network
    /// adapter. Held for the whole of a start, stop or re-scope, which
    /// can take a second.
    pub(crate) browse: Mutex<Option<MdnsController>>,
    /// Why the browse isn't running although Sendspin discovery is on:
    /// it failed to start (retried on each network check). `None` while
    /// it runs, is paused with its adapter, or discovery is off.
    pub(crate) discovery_error: Mutex<Option<String>>,
    pub(crate) source: Mutex<Option<SourceService>>,
    pub(crate) prompt: Mutex<Option<PairPrompt>>,
    pub(crate) disabled: AtomicBool,
    /// Speaker discovery may run this session (not `--no-discovery`).
    pub(crate) discovery_allowed: AtomicBool,
    /// Live [`MaOutputSession`]s (Music Assistant is the output while > 0).
    pub(crate) ma_output: Arc<AtomicUsize>,
}

/// Speaker-list id of the "Music Assistant" output.
pub const MA_OUTPUT_ID: &str = "music-assistant:input";

/// What the launch reconnect does with a saved Sendspin speaker or the
/// Music Assistant output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SendspinLaunch {
    /// Leave it alone.
    Skip(LaunchSkip),
    /// Select the Music Assistant output.
    MaOutput,
    /// Connect to this speaker once discovery has seen it.
    Speaker(String),
}

/// One check of the launch reconnect's wait for a saved Sendspin speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchStep {
    /// Not seen yet: check again shortly.
    Wait,
    /// Seen: select it.
    Select,
    /// Leave it alone, for this reason.
    Stop(&'static str),
}

/// Why the launch reconnect leaves the saved speaker alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchSkip {
    /// Reconnect at launch is off in the app settings.
    ReconnectOff,
    /// The speaker's own auto-connect setting is "Never".
    Never,
    /// Sendspin is off, or disabled for this run.
    SendspinOff,
    /// The Music Assistant input isn't running.
    InputOff,
}

/// Music Assistant selected as the output. While this session exists the
/// Music Assistant input reports itself available and sends this
/// computer's audio; dropping it stops both.
pub struct MaOutputSession {
    pub name: String,
    app: Weak<App>,
    selected: Arc<AtomicUsize>,
}

impl MaOutputSession {
    /// The paired Music Assistant server's address, when connected.
    pub fn ip(&self) -> IpAddr {
        self.app
            .upgrade()
            .and_then(|a| a.sendspin.source.lock().unwrap().as_ref().and_then(|s| s.paired_server_ip()))
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }

    pub fn stop(self) {}
}

impl Drop for MaOutputSession {
    fn drop(&mut self) {
        self.selected.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Persistence through the app's `config.json` (weak: sessions must not
/// keep the app alive).
struct AppStore(Weak<App>);

impl SendspinStore for AppStore {
    fn snapshot(&self) -> SendspinConfig {
        self.0
            .upgrade()
            .map(|a| a.user_config.lock().unwrap().sendspin.clone())
            .unwrap_or_default()
    }

    fn update(&self, f: &mut dyn FnMut(&mut SendspinConfig) -> bool) {
        if let Some(app) = self.0.upgrade() {
            let mut uc = app.user_config.lock().unwrap();
            if f(&mut uc.sendspin) {
                uc.save();
            }
        }
    }
}

/// Is this a Sendspin speaker id?
pub fn is_sendspin_id(id: &str) -> bool {
    id.starts_with(server::ID_PREFIX)
}

impl App {
    fn sendspin_store(self: &Arc<Self>) -> Arc<dyn SendspinStore> {
        Arc::new(AppStore(Arc::downgrade(self)))
    }

    /// Start Sendspin discovery and (if enabled) the Music Assistant
    /// source. `force_source` enables the source for this run without
    /// changing the saved setting (command-line use).
    pub fn init_sendspin(self: &Arc<Self>, discovery: bool, force_source: bool) {
        self.sendspin.discovery_allowed.store(discovery, Ordering::SeqCst);
        let (enabled, saved) = {
            let uc = self.user_config.lock().unwrap();
            (uc.sendspin.enabled, uc.sendspin.source_enabled)
        };
        if !enabled && !force_source {
            return;
        }
        if enabled {
            self.sync_sendspin_discovery();
        }
        if saved || force_source {
            if let Err(e) = self.start_sendspin_source() {
                self.record_error(format!("Couldn't start the Music Assistant input: {}", e));
            }
        }
    }

    /// Bring Sendspin speaker discovery in line with the settings: on
    /// while Sendspin is on (and discovery is allowed this run), browsing
    /// on the discovery network adapter and paused while that adapter is
    /// down, like AirPlay and UPnP discovery. Safe to call any time; slow
    /// when it restarts the browse, so not on the GUI thread.
    pub(crate) fn sync_sendspin_discovery(&self) {
        let mut browse = self.sendspin.browse.lock().unwrap();
        let wanted = self.sendspin.discovery_allowed.load(Ordering::SeqCst)
            && self.is_sendspin_available()
            && self.is_sendspin_enabled();
        if !wanted {
            *self.sendspin.discovery.lock().unwrap() = None;
            *self.sendspin.discovery_error.lock().unwrap() = None;
            if let Some(ctl) = browse.take() {
                ctl.stop();
            }
            return;
        }
        let ctl = browse.get_or_insert_with(|| {
            let st = SendspinDiscoveryState::new();
            *self.sendspin.discovery.lock().unwrap() = Some(st.clone());
            let found = st.clone();
            MdnsController::new(
                "Sendspin",
                move |d| server::spawn_discovery_on(found.clone(), d),
                move || st.clear(),
            )
        });
        let result = ctl.apply(&self.current_discovery_scope());
        let mut error = self.sendspin.discovery_error.lock().unwrap();
        match result {
            Ok(()) => {
                if error.take().is_some() {
                    if ctl.is_running() {
                        info!("Sendspin discovery: started");
                    } else {
                        info!("Sendspin discovery: paused (discovery adapter unavailable)");
                    }
                }
            }
            // Shown once per failure; the network check retries quietly.
            Err(e) if error.is_none() => {
                let msg = format!(
                    "Sendspin speaker discovery couldn't start ({:#}). It will keep trying.",
                    e
                );
                self.record_error(msg.clone());
                *error = Some(msg);
            }
            Err(e) => debug!("Sendspin discovery: still failing to start: {:#}", e),
        }
    }

    /// Network check: try again to start a Sendspin browse that failed to
    /// start (nothing else restarts it while the adapter scope holds).
    pub(crate) fn retry_failed_sendspin_discovery(&self) {
        if self.sendspin_discovery_error().is_some() {
            self.sync_sendspin_discovery();
        }
    }

    /// Why Sendspin speaker discovery is on but not running, for the window.
    pub(crate) fn sendspin_discovery_error(&self) -> Option<String> {
        self.sendspin.discovery_error.lock().unwrap().clone()
    }

    /// [`sync_sendspin_discovery`](Self::sync_sendspin_discovery) on a
    /// thread of its own.
    fn spawn_sendspin_discovery_sync(self: &Arc<Self>) {
        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-sendspin-discovery".into())
            .spawn(move || app.sync_sendspin_discovery())
            .ok();
    }

    fn sendspin_discovery(&self) -> Option<Arc<SendspinDiscoveryState>> {
        self.sendspin.discovery.lock().unwrap().clone()
    }

    /// The saved "Enable Sendspin" setting (off by default).
    pub fn is_sendspin_enabled(&self) -> bool {
        self.user_config.lock().unwrap().sendspin.enabled
    }

    /// GUI toggle: persist; on starts speaker discovery (and the Music
    /// Assistant input if it was left on); off stops the input, speaker
    /// discovery, any pairing prompt and a session to a Sendspin speaker.
    pub fn set_sendspin_enabled(self: &Arc<Self>, on: bool) {
        let source_saved = {
            let mut uc = self.user_config.lock().unwrap();
            if uc.sendspin.enabled != on {
                uc.sendspin.enabled = on;
                uc.save();
            }
            uc.sendspin.source_enabled
        };
        // Off the GUI thread: stopping the browse can take a second. Each
        // run applies the setting as saved by then, so quick toggles
        // settle on the last one.
        self.spawn_sendspin_discovery_sync();
        if on {
            if source_saved && !self.is_sendspin_source_enabled() {
                if let Err(e) = self.start_sendspin_source() {
                    self.record_error(format!("Couldn't start the Music Assistant input: {}", e));
                }
            }
        } else {
            self.stop_sendspin();
            *self.sendspin.prompt.lock().unwrap() = None;
            let session = {
                let mut g = self.session.lock().unwrap();
                if g.as_ref().is_some_and(|s| is_sendspin_id(&s.stable_id()) || s.stable_id() == MA_OUTPUT_ID) {
                    g.take()
                } else {
                    None
                }
            };
            if let Some(s) = session {
                info!("Sendspin turned off: disconnecting the Sendspin speaker");
                // Stopping joins the sender threads and can wait out a
                // stalled socket: keep it off the GUI thread.
                let _ = std::thread::Builder::new()
                    .name("stream-to-speaker-sendspin-stop".into())
                    .spawn(move || s.stop());
            }
        }
    }

    /// Turn off every Sendspin feature for this run (`--no-sendspin`).
    pub fn disable_sendspin(&self) {
        self.sendspin.disabled.store(true, Ordering::SeqCst);
    }

    pub fn is_sendspin_available(&self) -> bool {
        !self.sendspin.disabled.load(Ordering::SeqCst)
    }

    // ---- Music Assistant source (client role) ----

    /// Name this PC appears under in Music Assistant.
    pub fn sendspin_source_name(&self) -> String {
        self.user_config
            .lock()
            .unwrap()
            .sendspin
            .source_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(machine_name)
    }

    fn start_sendspin_source(self: &Arc<Self>) -> Result<(), String> {
        if !self.is_sendspin_available() {
            return Err("Sendspin is disabled for this run".into());
        }
        let mut slot = self.sendspin.source.lock().unwrap();
        if slot.is_some() {
            return Ok(());
        }
        let opts = SourceOptions {
            name: self.sendspin_source_name(),
            port: DEFAULT_CLIENT_PORT,
            software_version: crate::display_version().to_string(),
            advertise: true,
            output_selected: self.sendspin.ma_output.clone(),
        };
        let svc = SourceService::start(opts, self.hub.clone(), self.sendspin_store()).map_err(|e| format!("{:#}", e))?;
        info!("Music Assistant input enabled (port {})", svc.port());
        *slot = Some(svc);
        Ok(())
    }

    pub fn is_sendspin_source_enabled(&self) -> bool {
        self.sendspin.source.lock().unwrap().is_some()
    }

    /// GUI toggle: persist and start/stop the source.
    pub fn set_sendspin_source_enabled(self: &Arc<Self>, on: bool) -> Result<(), String> {
        {
            let mut uc = self.user_config.lock().unwrap();
            if uc.sendspin.source_enabled != on {
                uc.sendspin.source_enabled = on;
                uc.save();
            }
        }
        if on {
            self.start_sendspin_source()
        } else {
            let svc = self.sendspin.source.lock().unwrap().take();
            if let Some(svc) = svc {
                svc.stop();
            }
            self.end_ma_output();
            Ok(())
        }
    }

    pub fn sendspin_source_state(&self) -> Option<SourceState> {
        self.sendspin.source.lock().unwrap().as_ref().map(|s| s.state())
    }

    /// The code to show while Music Assistant pairs with us: (code, server).
    pub fn sendspin_source_code(&self) -> Option<(String, String)> {
        self.sendspin.source.lock().unwrap().as_ref().and_then(|s| s.pairing_code())
    }

    /// Recent pairing outcome text for the card, if any (last 15 s).
    pub fn sendspin_source_note(&self) -> Option<String> {
        self.sendspin
            .source
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.last_event())
            .filter(|(_, t)| t.elapsed() < Duration::from_secs(15))
            .map(|(m, _)| m)
    }

    /// Too many wrong pairing codes: code pairing is refused until the
    /// user allows it again.
    pub fn is_sendspin_pairing_paused(&self) -> bool {
        self.user_config.lock().unwrap().sendspin.pairing_code_failures >= PAIRING_FAILURE_LIMIT
    }

    /// GUI: reset the wrong-code count so code pairing works again.
    pub fn allow_sendspin_pairing_again(&self) {
        let mut uc = self.user_config.lock().unwrap();
        if uc.sendspin.pairing_code_failures != 0 {
            uc.sendspin.pairing_code_failures = 0;
            uc.save();
        }
    }

    /// Pairing token for Music Assistant's "pair with token" option.
    pub fn sendspin_pairing_token(self: &Arc<Self>) -> String {
        crate::sendspin::source::pairing_token(&*self.sendspin_store())
    }

    /// Music Assistant servers this PC is paired with (names).
    pub fn sendspin_paired_servers(&self) -> Vec<String> {
        self.user_config
            .lock()
            .unwrap()
            .sendspin
            .server_pairings
            .iter()
            .map(|r| r.server_name.clone().unwrap_or_else(|| "Music Assistant".into()))
            .collect()
    }

    pub fn stop_sendspin(&self) {
        let svc = self.sendspin.source.lock().unwrap().take();
        if let Some(svc) = svc {
            svc.stop();
        }
        self.end_ma_output();
    }

    /// Unselect Music Assistant as the output (its input went away).
    fn end_ma_output(&self) {
        let s = {
            let mut g = self.session.lock().unwrap();
            if g.as_ref().is_some_and(|s| s.stable_id() == MA_OUTPUT_ID) {
                g.take()
            } else {
                None
            }
        };
        if let Some(s) = s {
            info!("Music Assistant input stopped: unselecting it as the output");
            s.stop();
        }
    }

    /// Select Music Assistant as the output (speaker-list entry).
    pub(crate) fn start_ma_output(self: &Arc<Self>) -> Result<ActiveSession, String> {
        if !self.is_sendspin_available() {
            return Err("Sendspin is disabled for this run".into());
        }
        if !self.is_sendspin_source_enabled() {
            return Err("turn on \u{201c}Offer this computer as a Music Assistant input\u{201d} first".into());
        }
        // Counted, so an overlapping select that drops an older session
        // cannot unselect the newer one.
        self.sendspin.ma_output.fetch_add(1, Ordering::SeqCst);
        Ok(ActiveSession::MusicAssistant(MaOutputSession {
            name: "Music Assistant".into(),
            app: Arc::downgrade(self),
            selected: self.sendspin.ma_output.clone(),
        }))
    }

    /// The "Music Assistant" speaker-list entry: shown while the input is
    /// on and paired with a server (or while it is the output).
    fn ma_output_entry(&self, active_id: Option<&str>) -> Option<SpeakerInfo> {
        let active = active_id == Some(MA_OUTPUT_ID);
        let ip = self.sendspin.source.lock().unwrap().as_ref().map(|s| s.paired_server_ip());
        let paired = !self.user_config.lock().unwrap().sendspin.server_pairings.is_empty();
        let shown = active || (ip.is_some() && paired && self.is_sendspin_available());
        if !shown {
            return None;
        }
        Some(SpeakerInfo {
            active,
            id: MA_OUTPUT_ID.to_string(),
            friendly_name: "Music Assistant".to_string(),
            ip: ip.flatten().map(|a| a.to_string()).unwrap_or_default(),
            note: Some(
                "Sends this computer's audio to Music Assistant's Sendspin input, only while this \
                 is selected. Music Assistant plays it on the speaker set in this computer's \
                 \u{201c}Automatically play line-in on\u{201d} setting there, or wherever you start it."
                    .to_string(),
            ),
            manual: false,
            hidden: false,
        })
    }

    // ---- Sendspin speakers (server role) ----

    pub(crate) fn sendspin_speakers(&self, active_id: Option<&str>) -> Vec<SpeakerInfo> {
        let ma = self.ma_output_entry(active_id);
        let Some(d) = self.sendspin_discovery() else {
            return ma.into_iter().collect();
        };
        if !self.is_sendspin_available() || !self.is_sendspin_enabled() {
            return ma.into_iter().collect();
        }
        ma.into_iter().chain(d.renderers()
            .into_iter()
            .map(|r| {
                let id = r.stable_id();
                SpeakerInfo {
                    active: active_id == Some(id.as_str()),
                    id,
                    friendly_name: format!("{} (Sendspin)", r.friendly_name),
                    ip: r.ip.to_string(),
                    note: Some(
                        "Sendspin speaker (for example an ESPHome or Music Assistant speaker). \
                         Selecting it streams to it directly; if Music Assistant is playing on it \
                         at the time, this computer normally takes over."
                            .to_string(),
                    ),
                    manual: false,
                    hidden: false,
                }
            }))
            .collect()
    }

    /// What the launch reconnect does with the saved speaker when it is a
    /// Sendspin speaker or the Music Assistant output (`None`: it is
    /// neither). The same gates as for every other speaker: reconnect at
    /// launch is on, and the speaker's own auto-connect setting isn't
    /// "Never".
    pub(crate) fn sendspin_launch_plan(&self) -> Option<SendspinLaunch> {
        let id = self.saved_speaker_id().filter(|id| is_sendspin_id(id) || id == MA_OUTPUT_ID)?;
        Some(if !self.is_auto_reconnect_on_launch() {
            SendspinLaunch::Skip(LaunchSkip::ReconnectOff)
        } else if !self.auto_connect_allowed_at_launch(&id) {
            SendspinLaunch::Skip(LaunchSkip::Never)
        } else if id == MA_OUTPUT_ID {
            if self.is_sendspin_source_enabled() {
                SendspinLaunch::MaOutput
            } else {
                SendspinLaunch::Skip(LaunchSkip::InputOff)
            }
        } else if !self.is_sendspin_available() || !self.is_sendspin_enabled() {
            SendspinLaunch::Skip(LaunchSkip::SendspinOff)
        } else {
            SendspinLaunch::Speaker(id)
        })
    }

    /// Launch: reconnect to a saved Sendspin speaker once discovery has
    /// seen it (mDNS needs a moment), or select the Music Assistant
    /// output — see [`sendspin_launch_plan`](Self::sendspin_launch_plan).
    pub fn reconnect_saved_sendspin_speaker(self: &Arc<Self>) {
        let id = match self.sendspin_launch_plan() {
            None => return,
            Some(SendspinLaunch::Skip(why)) => {
                info!(
                    "auto-reconnect: saved {:?} left alone ({:?})",
                    self.saved_speaker_id().unwrap_or_default(),
                    why
                );
                return;
            }
            Some(SendspinLaunch::MaOutput) => {
                info!("auto-reconnect: Music Assistant output");
                self.select_speaker_async_with(MA_OUTPUT_ID, false, None);
                return;
            }
            Some(SendspinLaunch::Speaker(id)) => id,
        };
        let app = self.clone();
        let picks_seen = self.user_pick_count();
        std::thread::Builder::new()
            .name("stream-to-speaker-sendspin-launch".into())
            .spawn(move || {
                for _ in 0..100 {
                    if app.is_shutting_down() {
                        return;
                    }
                    match app.sendspin_launch_step(&id, picks_seen) {
                        LaunchStep::Wait => {}
                        LaunchStep::Stop(why) => {
                            info!("auto-reconnect: saved Sendspin speaker {:?}: {}", id, why);
                            return;
                        }
                        LaunchStep::Select => {
                            info!("auto-reconnect: saved Sendspin speaker {:?}", id);
                            app.connect_unless_user_picked(&id, picks_seen);
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                info!("auto-reconnect: saved Sendspin speaker {:?} not found", id);
            })
            .ok();
    }

    /// One check of the launch wait for the saved Sendspin speaker `id`,
    /// which started when [`user_pick_count`](Self::user_pick_count) was
    /// `picks_seen`: give up when the user picked a speaker since (even
    /// one that failed to connect), or the launch plan no longer selects
    /// `id` (Sendspin turned off, the speaker set to "Never", another
    /// speaker saved); select it once discovery has seen it. Automatic
    /// connects ("Always" speakers) don't count as a pick; one that binds
    /// makes its speaker the saved one, which ends the wait.
    pub(crate) fn sendspin_launch_step(&self, id: &str, picks_seen: u64) -> LaunchStep {
        if self.user_pick_count() != picks_seen {
            return LaunchStep::Stop("a speaker was picked meanwhile");
        }
        match self.sendspin_launch_plan() {
            Some(SendspinLaunch::Speaker(planned)) if planned == id => {}
            _ => return LaunchStep::Stop("no longer reconnected at launch"),
        }
        if self.sendspin_name_for(id).is_some() {
            LaunchStep::Select
        } else {
            LaunchStep::Wait
        }
    }

    /// Is Music Assistant the selected output?
    pub fn is_ma_output_selected(&self) -> bool {
        self.sendspin.ma_output.load(Ordering::SeqCst) > 0
    }

    pub(crate) fn sendspin_name_for(&self, id: &str) -> Option<String> {
        if id == MA_OUTPUT_ID {
            return Some("Music Assistant".into());
        }
        self.sendspin_discovery()?.find_by_id(id).map(|r| r.friendly_name)
    }

    /// Bring up a session to a Sendspin speaker.
    pub(crate) fn start_sendspin_player(
        self: &Arc<Self>,
        id: &str,
        pairing: Option<PairingInput>,
        interactive: bool,
    ) -> Result<ActiveSession, String> {
        if !self.is_sendspin_available() {
            return Err("Sendspin is disabled for this run".into());
        }
        if !self.is_sendspin_enabled() {
            return Err("Sendspin is turned off in settings".into());
        }
        let disc = self
            .sendspin_discovery()
            .filter(|_| self.sendspin_discovery_error().is_none())
            .ok_or_else(|| "Sendspin discovery is not running".to_string())?;
        let renderer = disc
            .find_by_id(id)
            .ok_or_else(|| format!("no Sendspin speaker with id {:?}", id))?;
        let name = renderer.friendly_name.clone();
        let extra = self.user_config.lock().unwrap().sendspin.player_extra_latency_ms;
        let weak = Arc::downgrade(self);
        // Track metadata from the OS media session, only while the
        // existing "forward now-playing" setting is on.
        let now_playing: NowPlayingFn = Arc::new(move || {
            let app = weak.upgrade()?;
            if !app.user_config.lock().unwrap().forward_now_playing {
                return None;
            }
            crate::now_playing::current().map(|np| (np.title, np.artist, np.album))
        });
        let cfg = PlayerSessionConfig {
            renderer,
            server_name: format!("{} on {}", PRODUCT_NAME, machine_name()),
            store: self.sendspin_store(),
            samples_rx: self.hub.subscribe(),
            extra_latency_ms: extra,
            pairing,
            now_playing: Some(now_playing),
            connect_timeout: Duration::from_secs(5),
        };
        match PlayerSession::start(cfg) {
            Ok(s) => {
                // Sendspin may have been turned off while we connected.
                if !self.is_sendspin_enabled() {
                    s.stop();
                    return Err("Sendspin was turned off in settings".into());
                }
                if let Some(v) = s.volume() {
                    self.vsync.prime_initial_volume(v);
                }
                Ok(ActiveSession::Sendspin(s))
            }
            Err(StartError::NeedsPairing {
                methods,
                lost_credential,
                identity_changed,
            }) => {
                let what = if identity_changed {
                    format!("{}'s identity changed since it was paired, so it must be paired again", name)
                } else {
                    format!("{} needs to be paired once", name)
                };
                if interactive {
                    self.begin_sendspin_pairing(id, &name, &methods, lost_credential, identity_changed);
                    Err(format!("{} — follow the prompt.", what))
                } else {
                    Err(format!(
                        "{} — open the {} window and click it in the speaker list.",
                        what, PRODUCT_NAME
                    ))
                }
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn begin_sendspin_pairing(self: &Arc<Self>, id: &str, name: &str, methods: &[PairMethod], lost: bool, identity_changed: bool) {
        let kind = if methods.contains(&PairMethod::DynamicCode) {
            PairPromptKind::LiveCode
        } else if methods.contains(&PairMethod::StaticCode) {
            PairPromptKind::StaticCodeOrToken
        } else {
            PairPromptKind::TokenOnly
        };
        let view = PairPromptView {
            id: id.to_string(),
            name: name.to_string(),
            kind,
            lost_credential: lost,
            identity_changed,
        };
        if kind == PairPromptKind::LiveCode {
            // Connect again in pairing mode: the speaker derives a code
            // for *this* connection and shows/speaks it; the prompt opens
            // when it does.
            let input = self.live_code_input(view);
            self.spawn_select_with_pairing(id.to_string(), input);
        } else {
            *self.sendspin.prompt.lock().unwrap() = Some(PairPrompt { view, tx: None });
        }
    }

    /// A `PairingInput::Code` whose `ask` publishes the prompt and waits
    /// for the user (3 minutes, the server-side attempt bound).
    fn live_code_input(self: &Arc<Self>, view: PairPromptView) -> PairingInput {
        let weak = Arc::downgrade(self);
        PairingInput::Code {
            dynamic: true,
            ask: Box::new(move || {
                let app = weak.upgrade()?;
                let (tx, rx) = bounded::<Option<String>>(1);
                *app.sendspin.prompt.lock().unwrap() = Some(PairPrompt {
                    view: view.clone(),
                    tx: Some(tx),
                });
                let answer = rx.recv_timeout(Duration::from_secs(175)).ok().flatten();
                let mut slot = app.sendspin.prompt.lock().unwrap();
                if slot.as_ref().map(|p| p.view.id == view.id && p.tx.is_some()).unwrap_or(false) {
                    *slot = None;
                }
                answer
            }),
        }
    }

    fn spawn_select_with_pairing(self: &Arc<Self>, id: String, input: PairingInput) {
        self.note_user_pick();
        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-sendspin-pair".into())
            .spawn(move || {
                // Wait out the connect that discovered the need to pair.
                for _ in 0..100 {
                    if app.connecting.lock().unwrap().is_none() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                app.select_speaker_async_with(&id, true, Some(input));
            })
            .ok();
    }

    /// GUI: the pending Sendspin pairing prompt, if any.
    pub fn pending_sendspin_pairing(&self) -> Option<PairPromptView> {
        self.sendspin.prompt.lock().unwrap().as_ref().map(|p| p.view.clone())
    }

    /// GUI: answer the pairing prompt (`None` = cancel). Accepts a code
    /// (digits; spaces and dashes ignored) or a pairing token (`SP:…`).
    pub fn submit_sendspin_pairing(self: &Arc<Self>, input: Option<String>) {
        let Some(prompt) = self.sendspin.prompt.lock().unwrap().take() else {
            return;
        };
        let id = prompt.view.id.clone();
        let text = input.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let is_token = text.as_deref().map(|t| t.to_ascii_uppercase().starts_with("SP:")).unwrap_or(false);
        match (prompt.tx, text) {
            (Some(tx), Some(t)) if !is_token => {
                let _ = tx.send(Some(t));
            }
            (Some(tx), Some(t)) => {
                // A token while a live-code attempt waits: end that
                // attempt, then pair with the token.
                let _ = tx.send(None);
                self.spawn_select_with_pairing(id, PairingInput::Token(t));
            }
            (Some(tx), None) => {
                let _ = tx.send(None);
            }
            (None, Some(t)) if is_token => self.spawn_select_with_pairing(id, PairingInput::Token(t)),
            (None, Some(t)) => {
                let code = t.clone();
                self.spawn_select_with_pairing(
                    id,
                    PairingInput::Code {
                        dynamic: false,
                        ask: Box::new(move || Some(code.clone())),
                    },
                );
            }
            (None, None) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert!(is_sendspin_id("sendspin:kitchen"));
        assert!(!is_sendspin_id("airplay:aa"));
    }
}
