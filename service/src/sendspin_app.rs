//! App glue for Sendspin: Sendspin players in the speaker list (we are the
//! server), the one-time pairing prompt for players that need it, and the
//! Music Assistant source (we are a client that Music Assistant plays as
//! a Live Input).

use crossbeam_channel::{bounded, Sender};
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::app::{ActiveSession, App};
use crate::http_server::SpeakerInfo;
use crate::sendspin::mdns::DEFAULT_CLIENT_PORT;
use crate::sendspin::proto::PairMethod;
use crate::sendspin::server::{
    self, NowPlayingFn, PairingInput, PlayerSession, PlayerSessionConfig, SendspinDiscoveryState, StartError,
};
use crate::sendspin::source::{SourceOptions, SourceService, SourceState};
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
}

pub(crate) struct PairPrompt {
    view: PairPromptView,
    /// Live-code attempts: the connect thread waits on the other end.
    tx: Option<Sender<Option<String>>>,
}

#[derive(Default)]
pub struct SendspinAppState {
    pub(crate) discovery: OnceLock<Arc<SendspinDiscoveryState>>,
    pub(crate) source: Mutex<Option<SourceService>>,
    pub(crate) prompt: Mutex<Option<PairPrompt>>,
    pub(crate) disabled: AtomicBool,
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
        if discovery {
            let st = SendspinDiscoveryState::new();
            match server::spawn_discovery(st.clone()) {
                Ok(()) => {
                    let _ = self.sendspin.discovery.set(st);
                }
                Err(e) => warn!("Sendspin discovery failed to start: {:#} (continuing without it)", e),
            }
        }
        let saved = self.user_config.lock().unwrap().sendspin.source_enabled;
        if saved || force_source {
            if let Err(e) = self.start_sendspin_source() {
                self.record_error(format!("Couldn't start the Music Assistant input: {}", e));
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
            if let Some(svc) = self.sendspin.source.lock().unwrap().take() {
                svc.stop();
            }
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
        if let Some(svc) = self.sendspin.source.lock().unwrap().take() {
            svc.stop();
        }
    }

    // ---- Sendspin speakers (server role) ----

    pub(crate) fn sendspin_speakers(&self, active_id: Option<&str>) -> Vec<SpeakerInfo> {
        let Some(d) = self.sendspin.discovery.get() else {
            return Vec::new();
        };
        if !self.is_sendspin_available() {
            return Vec::new();
        }
        d.renderers()
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
                         at the time, this computer takes over."
                            .to_string(),
                    ),
                }
            })
            .collect()
    }

    /// Launch: reconnect to a saved Sendspin speaker once discovery has
    /// seen it (mDNS needs a moment), when launch reconnect is on.
    pub fn reconnect_saved_sendspin_speaker(self: &Arc<Self>) {
        let Some(id) = self.saved_speaker_id().filter(|id| is_sendspin_id(id)) else {
            return;
        };
        if !self.is_auto_reconnect_on_launch() || !self.is_sendspin_available() {
            return;
        }
        let app = self.clone();
        std::thread::Builder::new()
            .name("stream-to-speaker-sendspin-launch".into())
            .spawn(move || {
                for _ in 0..100 {
                    if app.is_shutting_down() {
                        return;
                    }
                    if app.sendspin_name_for(&id).is_some() {
                        info!("auto-reconnect: saved Sendspin speaker {:?}", id);
                        app.select_speaker_async_with(&id, false, None);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                info!("auto-reconnect: saved Sendspin speaker {:?} not found", id);
            })
            .ok();
    }

    pub(crate) fn sendspin_name_for(&self, id: &str) -> Option<String> {
        self.sendspin.discovery.get()?.find_by_id(id).map(|r| r.friendly_name)
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
        let disc = self
            .sendspin
            .discovery
            .get()
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
                if let Some(v) = s.volume() {
                    self.vsync.prime_initial_volume(v);
                }
                Ok(ActiveSession::Sendspin(s))
            }
            Err(StartError::NeedsPairing { methods, lost_credential }) => {
                if interactive {
                    self.begin_sendspin_pairing(id, &name, &methods, lost_credential);
                    Err(format!("{} needs to be paired once — follow the prompt.", name))
                } else {
                    Err(format!(
                        "{} needs to be paired once — open the {} window and click it in the speaker list.",
                        name, PRODUCT_NAME
                    ))
                }
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn begin_sendspin_pairing(self: &Arc<Self>, id: &str, name: &str, methods: &[PairMethod], lost: bool) {
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
