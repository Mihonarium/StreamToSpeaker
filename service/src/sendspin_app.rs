//! App glue for Sendspin: Sendspin players in the speaker list (we are the
//! server), the one-time pairing prompt for players that need it, and the
//! Music Assistant source (we are a client that Music Assistant plays as
//! a Live Input).

use crossbeam_channel::{bounded, Sender};
use log::{debug, info};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::app::{ActiveSession, App};
use crate::discovery_net::MdnsController;
use crate::http_server::SpeakerInfo;
use crate::sendspin::mdns::DEFAULT_CLIENT_PORT;
use crate::sendspin::proto::PairMethod;
use crate::sendspin::server::{
    self, NowPlayingFn, PairingInput, PlayerSession, PlayerSessionConfig, SendspinDiscoveryState, StartError,
};
use crate::sendspin::source::{PortRefusal, SourceOptions, SourceService, SourceState, PAIRING_FAILURE_LIMIT};
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
    /// For a code the speaker shows now: until when it can be typed.
    pub until: Option<Instant>,
}

pub(crate) struct PairPrompt {
    view: PairPromptView,
    picks_seen: u64,
    /// Which attempt put it up: an attempt takes down only its own.
    attempt: u64,
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
    /// Held for the whole of a start or stop of the Music Assistant input,
    /// so they don't cross; `source` itself is held only to put the
    /// service in or take it out, so the window can read its state while a
    /// start waits.
    pub(crate) source_turning: Mutex<()>,
    pub(crate) prompt: Mutex<Option<PairPrompt>>,
    pub(crate) disabled: AtomicBool,
    /// Speaker discovery may run this session (not `--no-discovery`).
    pub(crate) discovery_allowed: AtomicBool,
    /// Live [`MaOutputSession`]s (Music Assistant is the output while > 0).
    pub(crate) ma_output: Arc<AtomicUsize>,
    /// The level this PC sends to Music Assistant, 0–100.
    pub(crate) ma_level: Arc<std::sync::atomic::AtomicU8>,
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

/// Most addresses offered for a Music Assistant on another network.
const MA_ADDRESSES: usize = 3;

/// The addresses to give a Music Assistant on another network for this
/// computer, most likely first: `first` (the address that names this
/// computer to other devices: the one given with `--advertise-ip`, the
/// discovery adapter's, the default route's…), then this computer's other
/// IPv4 addresses (`local`); [`MA_ADDRESSES`] at most, and never loopback,
/// link-local, unspecified, multicast or broadcast.
pub fn addresses_for_music_assistant(first: IpAddr, local: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let usable = |ip: &Ipv4Addr| {
        !(ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast())
    };
    let first = match first {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(_) => None,
    };
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for ip in first.into_iter().chain(local.iter().copied()).filter(usable) {
        if !out.contains(&ip) && out.len() < MA_ADDRESSES {
            out.push(ip);
        }
    }
    out
}

/// The Music Assistant input's ports (see
/// [`App::sendspin_source_ports`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePorts {
    pub port: u16,
    pub wanted: u16,
    pub refused: Option<PortRefusal>,
}

/// What the Music Assistant card says, while it waits, to someone whose
/// Music Assistant runs on another network, where it can't find this
/// computer by itself: paragraphs, and the address to offer for copying.
/// `addresses` from [`addresses_for_music_assistant`].
pub fn music_assistant_elsewhere(ports: &SourcePorts, addresses: &[Ipv4Addr]) -> (Vec<String>, Option<Ipv4Addr>) {
    let SourcePorts { port, wanted, refused } = ports;
    let mut text = vec![
        "Music Assistant finds this computer by itself only on the same network. If it runs on \
         another one (another VLAN or subnet), open the Sendspin provider's settings in Music \
         Assistant and add this computer's address under \u{201c}Manual IP addresses for \
         discovery\u{201d}. Music Assistant then connects to it, and keeps trying until it gets \
         through (a firewall between the networks has to let it)."
            .to_string(),
    ];
    if port != wanted {
        text.push(match refused {
            Some(PortRefusal::InUse) => format!(
                "Port {wanted}, where Music Assistant connects to an address it is given, is in use \
                 by another program, so this input listens on port {port}. Music Assistant on this \
                 network still finds it. For the address to work, close the program that uses port \
                 {wanted}, then turn this input off and on."
            ),
            Some(PortRefusal::StillClosing) => format!(
                "Port {wanted}, where Music Assistant connects to an address it is given, wasn't free \
                 yet when this input was turned on again, so it listens on port {port} for now. \
                 Music Assistant on this network still finds it. For the address to work, turn \
                 this input off and on again."
            ),
            Some(PortRefusal::Unavailable(why)) => format!(
                "Port {wanted}, where Music Assistant connects to an address it is given, can't be \
                 used on this computer right now, so this input listens on port {port}. Music \
                 Assistant on this network still finds it, but the address works only once port \
                 {wanted} can be used. Details: {why}"
            ),
            None => format!(
                "This input listens on port {port}, not on port {wanted}, where Music Assistant \
                 connects to an address it is given. Music Assistant on this network still finds it."
            ),
        });
    }
    let Some((first, others)) = addresses.split_first() else {
        text.push("This computer has no network address right now.".into());
        return (text, None);
    };
    text.push(format!("This computer's address: {first}"));
    if !others.is_empty() {
        let others: Vec<String> = others.iter().map(Ipv4Addr::to_string).collect();
        text.push(format!(
            "If Music Assistant reaches this computer through another network adapter, use that \
             one's address instead: {}.",
            others.join(", ")
        ));
    }
    (text, Some(*first))
}

/// Every pairing in `c` with a Music Assistant server or a Sendspin
/// speaker: its id and key.
fn paired_ids(c: &SendspinConfig) -> Vec<(String, String)> {
    let servers = c.server_pairings.iter().map(|r| (r.server_id.clone(), r.psk.clone()));
    servers.chain(c.player_pairings.iter().map(|(id, p)| (id.clone(), p.psk.clone()))).collect()
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
            let before = paired_ids(&uc.sendspin);
            if !f(&mut uc.sendspin) {
                return;
            }
            uc.save();
            let new = paired_ids(&uc.sendspin).into_iter().any(|id| !before.contains(&id));
            // Written as the lock goes.
            drop(uc);
            // A pairing made now that isn't kept has to be made again after a
            // restart: say so where it was made.
            if new && app.settings_unsaved() {
                app.record_error(
                    "Paired, but the pairing can't be saved with the settings: you'll need to \
                     pair again after the app restarts.",
                );
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
        if !enabled && !saved && !force_source {
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
    /// Assistant input if it was left on); off stops speaker discovery,
    /// any pairing prompt and a session to a Sendspin speaker.
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
            // Sendspin speakers only: the Music Assistant input has a
            // switch of its own.
            *self.sendspin.prompt.lock().unwrap() = None;
            let session = {
                let mut g = self.session.lock().unwrap();
                if g.as_ref().is_some_and(|s| is_sendspin_id(&s.stable_id())) {
                    g.take().map(|s| self.releasing(s, None))
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
        let _turning = self.sendspin.source_turning.lock().unwrap();
        if self.sendspin.source.lock().unwrap().is_some() {
            return Ok(());
        }
        let opts = SourceOptions {
            name: self.sendspin_source_name(),
            port: DEFAULT_CLIENT_PORT,
            software_version: crate::display_version().to_string(),
            advertise: true,
            output_selected: self.sendspin.ma_output.clone(),
            level: self.sendspin.ma_level.clone(),
        };
        let svc = SourceService::start(opts, self.hub.clone(), self.sendspin_store()).map_err(|e| format!("{:#}", e))?;
        info!("Music Assistant input enabled (port {})", svc.port());
        *self.sendspin.source.lock().unwrap() = Some(svc);
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
            let _turning = self.sendspin.source_turning.lock().unwrap();
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

    /// How many times a paired Music Assistant server has connected; zero
    /// while the input is off.
    pub fn sendspin_source_arrivals(&self) -> u64 {
        self.sendspin.source.lock().unwrap().as_ref().map_or(0, |s| s.arrivals())
    }

    #[cfg(test)]
    pub(crate) fn set_sendspin_source_state(&self, state: SourceState) {
        self.sendspin.source.lock().unwrap().as_ref().expect("the input is on").set_state(state);
    }

    /// The port the Music Assistant input listens on, the one it was meant
    /// to (8928, where Music Assistant dials an address it is given), and
    /// why it doesn't listen there; `None` while the input is off.
    pub fn sendspin_source_ports(&self) -> Option<SourcePorts> {
        self.sendspin.source.lock().unwrap().as_ref().map(|s| SourcePorts {
            port: s.port(),
            wanted: s.wanted_port(),
            refused: s.port_refusal(),
        })
    }

    /// The addresses a Music Assistant on another network can be given for
    /// this computer (see [`addresses_for_music_assistant`]).
    pub fn music_assistant_addresses(&self) -> Vec<Ipv4Addr> {
        addresses_for_music_assistant(self.this_pc_address(), &crate::ssdp::local_ipv4_interfaces())
    }

    /// The code to show while Music Assistant pairs with us: the code,
    /// the server, and when the attempt ends at the latest.
    pub fn sendspin_source_code(&self) -> Option<(String, String, Instant)> {
        self.sendspin.source.lock().unwrap().as_ref().and_then(|s| s.pairing_code())
    }

    #[cfg(test)]
    pub(crate) fn set_sendspin_source_code(&self, code: Option<(String, String, Instant)>) {
        self.sendspin.source.lock().unwrap().as_ref().expect("the input is on").set_pairing_code(code);
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
        let _turning = self.sendspin.source_turning.lock().unwrap();
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
                g.take().map(|s| self.releasing(s, None))
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
        if let Some(level) = crate::engine::cancel::welcome_level() {
            self.set_ma_level(level);
        }
        self.sendspin.ma_output.fetch_add(1, Ordering::SeqCst);
        Ok(ActiveSession::MusicAssistant(MaOutputSession {
            name: "Music Assistant".into(),
            app: Arc::downgrade(self),
            selected: self.sendspin.ma_output.clone(),
        }))
    }

    /// The "Music Assistant" speaker-list entry: listed while a paired
    /// server is connected (or while it is the output). A server that is
    /// away can't play what is sent to it, so it isn't offered then.
    fn ma_output_entry(&self, active_id: Option<&str>) -> Option<SpeakerInfo> {
        let active = active_id == Some(MA_OUTPUT_ID);
        let connected = matches!(self.sendspin_source_state(), Some(SourceState::Connected { .. }));
        if !active && !(connected && self.is_sendspin_available()) {
            return None;
        }
        let ip = self.sendspin.source.lock().unwrap().as_ref().and_then(|s| s.paired_server_ip());
        Some(SpeakerInfo {
            active,
            id: MA_OUTPUT_ID.to_string(),
            friendly_name: "Music Assistant".to_string(),
            ip: ip.map(|a| a.to_string()).unwrap_or_default(),
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
                self.connect_in_background(MA_OUTPUT_ID);
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

    fn sendspin_renderer(&self, id: &str) -> Result<server::SendspinRenderer, String> {
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
        disc
            .find_by_id(id)
            .ok_or_else(|| format!("no Sendspin speaker with id {:?}", id))
    }

    /// Bring up a session to a Sendspin speaker.
    pub(crate) fn start_sendspin_player(
        self: &Arc<Self>,
        id: &str,
        interactive: bool,
    ) -> Result<ActiveSession, String> {
        let picks_seen = crate::engine::cancel::binding().map_or_else(|| self.user_pick_count(), |b| b.picks_seen);
        let renderer = self.sendspin_renderer(id)?;
        let name = renderer.friendly_name.clone();
        // What was added to this speaker's delay (nothing can be removed).
        let extra = self.delay_for_sendspin(id);
        let weak = Arc::downgrade(self);
        // Track metadata from the OS media session, only while the
        // existing "forward now-playing" setting is on.
        let now_playing: NowPlayingFn = Arc::new(move || {
            let app = weak.upgrade()?;
            if !app.user_config.lock().unwrap().forward_now_playing {
                return None;
            }
            app.title_now().map(|np| (np.title, np.artist, np.album))
        });
        let cfg = PlayerSessionConfig {
            renderer,
            server_name: format!("{} on {}", PRODUCT_NAME, machine_name()),
            store: self.sendspin_store(),
            samples_rx: self.hub.subscribe(),
            extra_latency_ms: extra,
            pairing: None,
            now_playing: Some(now_playing),
            connect_timeout: Duration::from_secs(5),
        };
        let level = self.sendspin_start_level(id);
        match PlayerSession::start_with_local_level(cfg, level, self.vsync.is_muted()) {
            Ok(s) => {
                // Sendspin may have been turned off while we connected.
                if !self.is_sendspin_enabled() {
                    s.stop();
                    return Err("Sendspin was turned off in settings".into());
                }
                Ok(ActiveSession::Sendspin(s))
            }
            Err(StartError::NeedsPairing {
                methods,
                lost_credential,
                identity_changed,
            }) => {
                if crate::engine::cancel::cancelled() { return Err(crate::app::SUPERSEDED.into()); }
                let what = if identity_changed {
                    format!("{}'s identity changed since it was paired, so it must be paired again", name)
                } else {
                    format!("{} needs to be paired once", name)
                };
                if interactive {
                    self.begin_sendspin_pairing(id, &name, &methods, lost_credential, identity_changed, picks_seen);
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

    fn begin_sendspin_pairing(self: &Arc<Self>, id: &str, name: &str, methods: &[PairMethod], lost: bool, identity_changed: bool, picks_seen: u64) {
        if crate::engine::cancel::cancelled() { return; }
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
            until: None,
        };
        if kind == PairPromptKind::LiveCode {
            // Connect again in pairing mode: the speaker derives a code
            // for *this* connection and shows/speaks it; the prompt opens
            // when it does.
            let input = self.live_code_input(view, picks_seen);
            self.spawn_select_with_pairing(id.to_string(), input, picks_seen);
        } else {
            self.put_up_pair_prompt(view, picks_seen, None);
        }
    }

    /// Puts up a pairing prompt and returns its attempt's number, by which
    /// the attempt takes it down again. Where the prompt of a later choice
    /// is up, nothing changes and `None` comes back: an attempt that this
    /// choice left behind must not take its place.
    fn put_up_pair_prompt(&self, view: PairPromptView, picks_seen: u64, tx: Option<Sender<Option<String>>>) -> Option<u64> {
        static ATTEMPTS: AtomicU64 = AtomicU64::new(1);
        let mut slot = self.sendspin.prompt.lock().unwrap();
        if slot.as_ref().is_some_and(|p| p.picks_seen > picks_seen) {
            return None;
        }
        let attempt = ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        *slot = Some(PairPrompt { view, picks_seen, attempt, tx });
        Some(attempt)
    }

    /// A `PairingInput::Code` whose `ask` publishes the prompt and waits
    /// for the user until shortly before the attempt ends (3 minutes after
    /// it began, the server-side bound), so that a code typed at the last
    /// moment can still be checked.
    fn live_code_input(self: &Arc<Self>, view: PairPromptView, picks_seen: u64) -> PairingInput {
        let weak = Arc::downgrade(self);
        PairingInput::Code {
            dynamic: true,
            ask: Box::new(move |ends: Instant| {
                let app = weak.upgrade()?;
                let until = ends.checked_sub(Duration::from_secs(5)).unwrap_or(ends);
                let (tx, rx) = bounded::<Option<String>>(1);
                let shown = PairPromptView { until: Some(until), ..view.clone() };
                let attempt = app.put_up_pair_prompt(shown, picks_seen, Some(tx))?;
                // A later attempt's prompt in its place ends this wait
                // unanswered, and stays up.
                let answer = rx.recv_deadline(until).ok().flatten();
                let mut slot = app.sendspin.prompt.lock().unwrap();
                if slot.as_ref().is_some_and(|p| p.attempt == attempt) {
                    *slot = None;
                }
                answer
            }),
        }
    }

    fn spawn_select_with_pairing(self: &Arc<Self>, id: String, input: PairingInput, picks_seen: u64) {
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
                let paired = app.sendspin_renderer(&id).and_then(|renderer| {
                    let name = format!("{} on {}", PRODUCT_NAME, machine_name());
                    server::pair_player(&renderer, &name, &app.sendspin_store(), input, Duration::from_secs(5))
                        .map_err(|e| e.to_string())
                });
                match paired {
                    Ok(()) if app.command_sink.get().is_some() => app.play_after_pairing(&id, picks_seen),
                    Ok(()) => app.connect_unless_user_picked(&id, picks_seen),
                    Err(e) => {
                        let msg = format!("Couldn't connect to speaker: {e}");
                        app.record_fault(&msg);
                        app.record_error(msg);
                    }
                }
            })
            .ok();
    }

    /// GUI: the pending Sendspin pairing prompt, if any.
    pub fn pending_sendspin_pairing(&self) -> Option<PairPromptView> {
        self.sendspin.prompt.lock().unwrap().as_ref().map(|p| p.view.clone())
    }

    /// GUI: answer the pairing prompt (`None` = cancel). Accepts a code
    /// (digits; spaces and dashes ignored) or a pairing token (`SP:…`).
    pub fn submit_sendspin_pairing(self: &Arc<Self>, id: &str, input: Option<String>) {
        let Some(prompt) = self.sendspin.prompt.lock().unwrap().take_if(|p| p.view.id == id) else {
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
                self.spawn_select_with_pairing(id, PairingInput::Token(t), prompt.picks_seen);
            }
            (Some(tx), None) => {
                let _ = tx.send(None);
            }
            (None, Some(t)) if is_token => self.spawn_select_with_pairing(id, PairingInput::Token(t), prompt.picks_seen),
            (None, Some(t)) => {
                let code = t.clone();
                self.spawn_select_with_pairing(
                    id,
                    PairingInput::Code {
                        dynamic: false,
                        ask: Box::new(move |_| Some(code.clone())),
                    },
                    prompt.picks_seen,
                );
            }
            (None, None) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_view(id: &str, kind: PairPromptKind) -> PairPromptView {
        PairPromptView { id: id.into(), name: id.into(), kind, lost_credential: false, identity_changed: false, until: None }
    }

    #[test]
    fn an_attempt_of_an_earlier_choice_leaves_a_later_ones_prompt_up() {
        let app = App::for_tests();
        assert!(app.put_up_pair_prompt(prompt_view("sendspin:den", PairPromptKind::TokenOnly), 2, None).is_some());
        assert_eq!(app.put_up_pair_prompt(prompt_view("sendspin:hall", PairPromptKind::TokenOnly), 1, None), None);
        assert_eq!(app.pending_sendspin_pairing().unwrap().id, "sendspin:den");
        // The next choice's prompt takes its place.
        assert!(app.put_up_pair_prompt(prompt_view("sendspin:hall", PairPromptKind::TokenOnly), 3, None).is_some());
        assert_eq!(app.pending_sendspin_pairing().unwrap().id, "sendspin:hall");
    }

    #[test]
    fn a_replaced_live_code_attempt_leaves_the_newer_prompt_up() {
        let app = App::for_tests();
        let ask = |picks_seen| match app.live_code_input(prompt_view("sendspin:den", PairPromptKind::LiveCode), picks_seen) {
            PairingInput::Code { ask, .. } => ask,
            _ => unreachable!(),
        };
        let (first, second) = (ask(1), ask(2));
        let ends = Instant::now() + Duration::from_secs(60);
        let old = std::thread::spawn(move || first(ends));
        assert!(server::test_player::eventually(|| app.pending_sendspin_pairing().is_some()));
        let new = std::thread::spawn(move || second(ends));
        // The second attempt's prompt replaces the first one's, whose wait
        // ends unanswered and takes nothing down.
        assert_eq!(old.join().unwrap(), None);
        assert_eq!(app.pending_sendspin_pairing().map(|p| p.id), Some("sendspin:den".to_string()));
        app.submit_sendspin_pairing("sendspin:den", Some("123456".into()));
        assert_eq!(new.join().unwrap(), Some("123456".to_string()));
        assert!(app.pending_sendspin_pairing().is_none());
    }

    #[test]
    fn typing_the_players_code_pairs_without_sound_and_requests_one_current_switch() {
        use crate::model::{Command, SpeakerId, Surface};
        use crate::sendspin::{channel, handshake, keys::b64url, noise, pairing, proto, pump, ws};
        use serde_json::json;
        let wait = Duration::from_secs(10);
        for picked in [false, true] {
            let app = App::for_tests();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let renderer = server::SendspinRenderer {
                instance: "Den".into(), friendly_name: "Den".into(), ip: addr.ip(), port: addr.port(), path: "/sendspin".into(),
            };
            let id = renderer.stable_id();
            let discovery = SendspinDiscoveryState::new();
            discovery.insert(renderer);
            *app.sendspin.discovery.lock().unwrap() = Some(discovery);
            app.user_config.lock().unwrap().sendspin.enabled = true;
            let (show, code) = bounded(1);
            let peer = std::thread::spawn(move || {
                let key = noise::generate_private_key();
                // First it asks to pair. On the second connection it shows the code.
                for attempt in 0..2 {
                    let (stream, _) = listener.accept().unwrap();
                    let ws::Accepted::WebSocket(r, w) = ws::accept(stream, "/sendspin", wait).unwrap() else { panic!() };
                    let hs = handshake::client_handshake(r, w, &key, noise::Suite::ChaChaPoly, &|_, _| None, wait).unwrap();
                    let events = pump::spawn_pump(hs.reader, "den", Box::new(|_, _| None));
                    assert!(matches!(events.recv_timeout(wait).unwrap(), pump::Event::Json { value, .. } if channel::msg_type(&value) == "server/hello"));
                    hs.writer.send_json(&channel::envelope("client/hello", json!({
                        "name": "Den", "supported_roles": ["player@v1"], "supported_pair_methods": {"dynamic_pairing_code": {}}
                    }))).unwrap();
                    if attempt == 0 {
                        assert!(matches!(events.recv_timeout(wait).unwrap(), pump::Event::Closed(_)));
                        continue;
                    }
                    let pump::Event::Json { value, .. } = events.recv_timeout(wait).unwrap() else { panic!() };
                    let activation = proto::parse_activation(channel::payload(&value));
                    assert!(activation.pairing && !activation.playback);
                    let ctx = pairing::PairingCtx { writer: &hs.writer, events: &events, dialect: proto::Dialect::Spec,
                        handshake_hash: hs.handshake_hash, suite: hs.suite, pairing_index: 1 };
                    let began = std::time::Instant::now();
                    let result = pairing::client_dynamic_code(&ctx, 6, &|shown| {
                        if let Some((digits, ends)) = shown {
                            // The code comes with the end of its attempt.
                            assert!(ends > began && ends <= std::time::Instant::now() + pairing::CLIENT_ATTEMPT_TIMEOUT);
                            show.send(digits).unwrap();
                        }
                    }).unwrap();
                    let pairing::PairOutcome::Finalized { long_term_psk } = result else { panic!("{result:?}") };
                    // The next frame must close the connection. Playback and audio never started.
                    assert!(matches!(events.recv_timeout(wait).unwrap(), pump::Event::Closed(_)));
                    return (b64url(&noise::x25519_public(&key)), b64url(&long_term_psk));
                }
                unreachable!()
            });
            let (sent, commands) = crossbeam_channel::unbounded();
            app.set_command_sink(Box::new(move |surface, command| {
                sent.send((surface, command)).unwrap();
                crate::model::command::ticket().1
            }));
            let picks_seen = app.user_pick_count();
            let binding = Arc::new(crate::engine::cancel::Binding::new(SpeakerId::new(&id), None, picks_seen, || {}));
            let checking = binding.enter();
            // The driver records the next choice before it cancels this connect.
            if picked { app.note_user_pick(); }
            assert!(app.start_sendspin_player(&id, true).is_err());
            if picked { app.release_job(&binding); }
            drop(checking);
            let digits = code.recv_timeout(wait).unwrap();
            assert!(server::test_player::eventually(|| app.pending_sendspin_pairing().is_some_and(|p| p.id == id)));
            // The prompt says until when the code can be typed: shortly
            // before the attempt's three minutes are over.
            let until = app.pending_sendspin_pairing().unwrap().until.expect("a live code's prompt has an end");
            let left = until.saturating_duration_since(std::time::Instant::now());
            assert!(left > Duration::from_secs(120) && left <= Duration::from_secs(175), "{left:?}");
            let port = crate::settings::real::AppSettings::new(app.clone());
            use crate::settings::{Act, SettingsPort};
            port.act(Act::SubmitSendspin { id: "elsewhere".into(), code: digits.clone() });
            port.act(Act::CancelSendspin("elsewhere".into()));
            assert_eq!(app.pending_sendspin_pairing().unwrap().id, id);
            port.act(Act::SubmitSendspin { id: id.clone(), code: digits });
            drop(port);
            let (client, psk) = peer.join().unwrap();
            assert_eq!(app.sendspin_store().snapshot().player_pairings[&client].psk, psk);
            assert!(!app.is_speaker_bound());
            drop(app);
            if !picked {
                assert_eq!(commands.recv_timeout(wait).unwrap(), (Surface::Panel, Command::PlayAfterPairing {
                    speaker: SpeakerId::new(&id), picks_seen,
                }));
            }
            assert_eq!(commands.recv_timeout(wait), Err(crossbeam_channel::RecvTimeoutError::Disconnected));
        }
    }

    #[test]
    fn a_cancelled_sendspin_connect_opens_no_pairing_prompt() {
        let app = App::for_tests();
        let _scope = crate::engine::cancel::enter(Arc::new(|| true));
        app.begin_sendspin_pairing("sendspin:den", "Den", &[PairMethod::StaticCode], false, false, app.user_pick_count());
        assert!(app.pending_sendspin_pairing().is_none());
    }

    #[test]
    fn ids() {
        assert!(is_sendspin_id("sendspin:kitchen"));
        assert!(!is_sendspin_id("airplay:aa"));
    }

    /// Earlier versions ran the Music Assistant input only while Sendspin
    /// was on as well. After the update the input runs where it ran
    /// before, and nowhere else; settings this version saved keep the
    /// input's own switch as it was.
    #[test]
    fn after_an_update_the_music_assistant_input_runs_only_where_it_ran_before() {
        use crate::user_config::UserConfig;
        let dir = std::env::temp_dir().join(format!("sts-sendspin-input-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The app as it starts with the settings file at `path`. Sendspin
        // is off for the run, so that starting the input shows as the
        // error it records instead of opening a port and advertising.
        let start = |path: &std::path::Path| {
            let app = App::with_settings(App::test_config(), UserConfig::load_at_path(path.into()), None, None);
            app.disable_sendspin();
            app.init_sendspin(false, false);
            let started = app
                .current_error()
                .is_some_and(|e| e.starts_with("Couldn't start the Music Assistant input"));
            (app, started)
        };
        let file = |name: &str, sendspin_on: bool| {
            let path = dir.join(name);
            let text = format!(r#"{{"sendspin": {{"enabled": {sendspin_on}, "source_enabled": true}}}}"#);
            std::fs::write(&path, text).unwrap();
            path
        };
        let input_saved = |path: &std::path::Path| {
            let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            saved["sendspin"]["source_enabled"] == true
        };

        // Sendspin was off, the input left ticked from before: it doesn't
        // start, isn't on in the settings and stays off on the next start.
        let path = file("sendspin-off.json", false);
        let (app, started) = start(&path);
        assert!(!started, "{:?}", app.current_error());
        assert!(!app.user_config.lock().unwrap().sendspin.source_enabled);
        assert!(!input_saved(&path));
        assert!(!start(&path).1);

        // Both were on: the input starts, as it did before the update.
        let path = file("both-on.json", true);
        let (app, started) = start(&path);
        assert!(started);
        assert!(app.user_config.lock().unwrap().sendspin.source_enabled);
        assert!(input_saved(&path));
        // Sendspin speakers turned off since: the input keeps running.
        app.set_sendspin_enabled(false);
        assert!(start(&path).1);

        // A new install: no input, and nothing saved at the start.
        let path = dir.join("new.json");
        let (app, started) = start(&path);
        assert!(!started && !app.user_config.lock().unwrap().sendspin.source_enabled);
        assert!(!path.exists());
        // The input turned on with Sendspin off, then a restart: it starts.
        assert!(app.set_sendspin_source_enabled(true).is_err(), "Sendspin is off for the run");
        assert!(input_saved(&path));
        assert!(start(&path).1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn music_assistant_elsewhere_is_offered_this_computers_own_addresses() {
        let ip = |s: &str| -> Ipv4Addr { s.parse().unwrap() };
        let local = [ip("10.0.0.5"), ip("169.254.3.4"), ip("192.168.1.20"), ip("172.16.0.9"), ip("127.0.0.1")];
        // The address that names this PC first, then the others; three at
        // most; no loopback or link-local, nothing twice.
        assert_eq!(
            addresses_for_music_assistant(IpAddr::V4(ip("192.168.1.20")), &local),
            [ip("192.168.1.20"), ip("10.0.0.5"), ip("172.16.0.9")]
        );
        // No network: loopback is what names the PC then, and is no answer.
        assert!(addresses_for_music_assistant(IpAddr::V4(Ipv4Addr::LOCALHOST), &[]).is_empty());
        assert_eq!(
            addresses_for_music_assistant("fe80::1".parse().unwrap(), &[ip("10.0.0.5")]),
            [ip("10.0.0.5")]
        );
        for bad in ["0.0.0.0", "224.0.0.251", "255.255.255.255", "169.254.1.1"] {
            assert!(addresses_for_music_assistant(IpAddr::V4(ip(bad)), &[]).is_empty(), "{bad}");
        }
    }

    #[test]
    fn the_card_tells_music_assistant_elsewhere_what_it_needs() {
        let ip = |s: &str| -> Ipv4Addr { s.parse().unwrap() };
        let ports = |port: u16, refused: Option<PortRefusal>| SourcePorts { port, wanted: 8928, refused };
        let (text, copy) = music_assistant_elsewhere(&ports(8928, None), &[ip("192.168.1.20"), ip("10.0.0.5")]);
        assert_eq!(copy, Some(ip("192.168.1.20")));
        assert!(text[0].contains("\u{201c}Manual IP addresses for discovery\u{201d}"), "{}", text[0]);
        assert_eq!(text[1], "This computer's address: 192.168.1.20");
        assert!(text[2].ends_with("use that one's address instead: 10.0.0.5."), "{}", text[2]);
        // Nothing says an entry may carry a port.
        assert!(text.iter().all(|t| !t.contains(":8928")));
        let (text, copy) = music_assistant_elsewhere(&ports(8928, None), &[ip("192.168.1.20")]);
        assert_eq!((text.len(), copy), (2, Some(ip("192.168.1.20"))));
        let (text, copy) = music_assistant_elsewhere(&ports(8928, None), &[]);
        assert_eq!(copy, None);
        assert_eq!(text[1], "This computer has no network address right now.");
        // Another program holds the port: it is named, with what to do; the
        // port used and the address stay shown.
        let (text, copy) = music_assistant_elsewhere(&ports(50123, Some(PortRefusal::InUse)), &[ip("192.168.1.20")]);
        assert_eq!(copy, Some(ip("192.168.1.20")));
        assert!(text[1].contains("is in use by another program, so this input listens on port 50123"), "{}", text[1]);
        assert!(text[1].contains("close the program that uses port 8928"), "{}", text[1]);
        assert_eq!(text[2], "This computer's address: 192.168.1.20");
        // Refused for another reason (a range Windows reserved): no culprit,
        // nothing to close, the system's words.
        let reserved = PortRefusal::Unavailable("An attempt was made to access a socket … (os error 10013)".into());
        let (text, copy) = music_assistant_elsewhere(&ports(50123, Some(reserved)), &[ip("192.168.1.20")]);
        assert_eq!(copy, Some(ip("192.168.1.20")));
        assert!(text[1].contains("can't be used on this computer right now"), "{}", text[1]);
        assert!(text[1].contains("listens on port 50123") && text[1].ends_with("(os error 10013)"), "{}", text[1]);
        assert!(!text[1].contains("program") && !text[1].contains("close"), "{}", text[1]);
        // This input's own port, not free yet after a quick off and on: no
        // other program is blamed.
        let (text, copy) = music_assistant_elsewhere(&ports(50123, Some(PortRefusal::StillClosing)), &[ip("192.168.1.20")]);
        assert_eq!(copy, Some(ip("192.168.1.20")));
        assert!(text[1].contains("wasn't free yet when this input was turned on again"), "{}", text[1]);
        assert!(text[1].contains("turn this input off and on again") && !text[1].contains("program"), "{}", text[1]);
    }

    #[test]
    fn a_pairing_that_cannot_be_saved_says_so() {
        use crate::sendspin::store::{PlayerPairing, ServerPairing};
        use crate::user_config::UserConfig;
        let app = App::with_settings(App::test_config(), UserConfig::unsaveable(), None, None);
        let store = app.sendspin_store();
        // A setting that isn't a pairing: the settings strip says enough.
        store.update(&mut |c| {
            c.pairing_code_failures += 1;
            true
        });
        assert_eq!(app.current_error(), None);
        // Music Assistant pairs, then a speaker does.
        let server = ServerPairing {
            server_id: "srv".into(),
            psk: "a".repeat(43),
            server_name: None,
            last_used_unix: 1,
        };
        store.update(&mut |c| {
            c.store_server_pairing(server.clone());
            true
        });
        assert!(app.current_error().unwrap().contains("pair again after the app restarts"));
        app.dismiss_error();
        store.update(&mut |c| {
            c.player_pairings.insert("p".into(), PlayerPairing { psk: "b".repeat(43), name: None, instance: None });
            true
        });
        assert!(app.current_error().is_some());
        // With settings that save, no message.
        let fine = App::for_tests();
        fine.sendspin_store().update(&mut |c| {
            c.store_server_pairing(server.clone());
            true
        });
        assert_eq!(fine.current_error(), None);
    }
}
