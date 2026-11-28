//! What Windows does for the engine, behind small traits: which output
//! Windows plays through and switching it, the outputs' names and levels,
//! which apps are playing where, the players its media keys reach, playing
//! sound on one of the PC's own outputs, this PC's network connections,
//! whether a screen reader runs, this app's own device, being told of
//! sleep, wake and sign-out, and handing the sound back if the app dies.
//!
//! Nothing here calls Windows. The Windows implementation lives beside the
//! code it calls; [`Platform::passive`] stands in where there is none (it
//! knows nothing and switches nothing, so the person keeps choosing the
//! output in Windows), and [`fake::FakePlatform`] plays Windows in tests.

use std::sync::Arc;

pub use crate::model::facts::OutputKind;
pub use crate::model::PcOutputId;

pub mod fake;
#[cfg(any(windows, test))]
#[path = "win/repair_policy.rs"]
mod repair_policy;
mod hotkeys;
pub use hotkeys::{DelayKeys, DelayPress, Hotkeys, KeyRegistration, KeySink, KeyStatus};
#[cfg(windows)]
pub(crate) mod win;
#[cfg(windows)]
pub use win::repair_output;

// Compile the same process matching, adapter classification and PCM code on test hosts.
#[cfg(all(test, not(windows)))]
#[path = "win/players.rs"]
mod players;
#[cfg(all(test, not(windows)))]
#[path = "win/network.rs"]
mod network;
#[cfg(all(test, not(windows)))]
#[path = "win/pass.rs"]
mod pass;

/// One of Windows' sound outputs, with Windows' own names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInfo {
    pub id: PcOutputId,
    /// Windows' full name, e.g. "Speakers (Realtek Audio)".
    pub friendly: String,
    /// The endpoint's own name, e.g. "Speakers", "Headphones".
    pub name: String,
    /// The device it belongs to, e.g. "Realtek Audio".
    pub device: String,
    pub kind: OutputKind,
}

impl OutputInfo {
    /// The model's view of it.
    pub fn to_model(&self) -> crate::model::facts::PcOutput {
        crate::model::facts::PcOutput {
            id: self.id.clone(),
            friendly: self.friendly.clone(),
            name: self.name.clone(),
            device: self.device.clone(),
            kind: self.kind,
        }
    }
}

/// The roles Windows keeps a default output for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Console,
    Multimedia,
    Communications,
}

/// The roles choosing a speaker moves: calls stay on this PC unless the
/// person chose otherwise.
pub const PLAYBACK_ROLES: &[Role] = &[Role::Console, Role::Multimedia];
/// The same with calls.
pub const ALL_ROLES: &[Role] = &[Role::Console, Role::Multimedia, Role::Communications];

/// Windows' outputs: which one plays, switching it, names and levels.
pub trait Outputs: Send + Sync {
    /// The default Windows reports for one role.
    fn default_for(&self, role: Role) -> Option<PcOutputId>;
    /// Windows' default output (the console role), or `None` when this
    /// platform can't tell.
    fn default_output(&self) -> Option<PcOutputId> { self.default_for(Role::Console) }
    /// The output used by apps that follow Windows' default for calls.
    fn calls_output(&self) -> Option<PcOutputId> { self.default_for(Role::Communications) }
    /// This app's own output (its driver's endpoint), while Windows has it.
    fn our_output(&self) -> Option<PcOutputId>;
    /// Windows' names for an output; `None` once it is gone.
    fn describe(&self, id: &PcOutputId) -> Option<OutputInfo>;
    /// Read the endpoint's friendly name when opening it for capture.
    fn current_friendly(&self, id: &PcOutputId) -> Option<String> {
        self.describe(id).map(|o| o.friendly)
    }
    /// Every output that is switched on, this app's own among them.
    fn list(&self) -> Vec<OutputInfo>;
    /// Make `id` Windows' default output for `roles`. The caller reads the
    /// default back to know whether Windows took it.
    fn set_default(&self, id: &PcOutputId, roles: &[Role]) -> Result<(), String>;
    /// An output's level (0–100) and whether it is muted.
    fn volume(&self, id: &PcOutputId) -> Option<(u8, bool)>;
    fn set_volume(&self, id: &PcOutputId, level: u8, muted: bool) -> Result<(), String>;
}

/// The key an app is known by in this seam, made from its program's file
/// name: lower case, without a folder and without ".exe" (`spotify` for
/// `C:\…\Spotify.exe`). An app's sound ([`AppAudio::key`]) and its player
/// ([`Player::app`]) carry the same key, which is how the two are matched.
pub fn app_key(program: &str) -> String {
    let file = program.rsplit(['\\', '/']).next().unwrap_or(program).trim();
    let lower = file.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

/// An app's sound on one output.
#[derive(Clone, Debug, PartialEq)]
pub struct AppAudio {
    /// The app's name as Windows shows it ("Spotify").
    pub app: String,
    /// The app's key ([`app_key`]); empty for Windows' system sounds and
    /// where the program can't be read.
    pub key: String,
    pub output: PcOutputId,
    /// The loudest peak since the last look, 0.0–1.0.
    pub peak: f32,
    /// Windows' own system sounds (a notification's ding).
    pub system_sounds: bool,
}

/// Which apps make sound on which output.
pub trait AudioSessions: Send + Sync {
    /// Every app's audio session on every active output, with its loudest
    /// peak since the last call.
    fn sessions(&self) -> Vec<AppAudio>;
    /// The level on an output now, 0.0–1.0 (the panel's meter).
    fn peak(&self, output: &PcOutputId) -> f32;
}

/// What a player does, as Windows' media sessions tell it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlayerStatus {
    Playing,
    Paused,
    Stopped,
    /// Opening, changing track, closed, or not said.
    Other,
}

/// A player Windows' media keys reach (a media session).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    /// Names this session among those in the list, for as long as it
    /// lasts.
    pub id: String,
    /// Its app's key ([`app_key`]), the same as [`AppAudio::key`] of that
    /// app's sound. `None` where Windows names the session in a way that
    /// can't be matched to a program: such a player is never matched to any
    /// sound.
    pub app: Option<String>,
    /// Windows' name for the app (*Spotify*), where it gives one.
    pub name: Option<String>,
    pub status: PlayerStatus,
    /// What it plays, where it says.
    pub title: Option<crate::now_playing::NowPlaying>,
}

/// The players Windows' media keys reach.
pub trait Players: Send + Sync {
    /// Every media session now.
    fn list(&self) -> Vec<Player>;
    /// Pause it, as pause pressed in the player would.
    fn pause(&self, id: &str) -> Result<(), String>;
    /// Start it, as play pressed in the player would.
    fn play(&self, id: &str) -> Result<(), String>;
}

/// Sound played on one of the PC's own outputs while Windows sends
/// everything to this app's device.
pub trait PassThrough: Send + Sync {
    /// Play what [`write`](PassThrough::write) gives on `output` from now
    /// on, in place of any output opened before.
    fn open(&self, output: &PcOutputId) -> Result<(), String>;
    /// Interleaved 16-bit frames at `rate` with `channels`. Returns at
    /// once: what the output can't take in time is dropped. Nothing
    /// happens while no output is open.
    fn write(&self, frames: &[i16], rate: u32, channels: u16);
    /// Stop playing and let the output go.
    fn close(&self);
}

/// What a network connection is.
pub use crate::model::situation::ConnKind;

/// One of this PC's network connections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetConnection {
    /// Windows' id for the network's profile: the same network has the
    /// same id each time this PC joins it.
    pub id: String,
    /// Its name, where Windows gives one.
    pub name: Option<String>,
    pub kind: ConnKind,
    /// Windows' id of the adapter the connection is on, where it is known.
    pub adapter: Option<String>,
    /// Whether Windows treats the network as private (`true`) or public
    /// (`false`); `None` where it can't be told or is neither (a domain's
    /// network).
    pub private: Option<bool>,
}

/// This PC's network connections.
pub trait Network: Send + Sync {
    /// The connections that are up now; empty when this PC has none.
    fn connections(&self) -> Vec<NetConnection>;
}

/// What the app needs to know about the person's desktop.
pub trait Desk: Send + Sync {
    /// A screen reader runs.
    fn screen_reader(&self) -> bool;
}

pub(crate) const REPAIR_TIMEOUT_MS: u32 = 120_000;

/// The elevated process has exited or Windows declined to start it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RepairExit {
    Success,
    Cancelled,
    #[default]
    Failed,
}

/// This app's own output in Windows.
pub trait OurDevice: Send + Sync {
    fn can_repair(&self) -> bool { false }
    fn turned_off(&self) -> bool;
    fn turn_on(&self) -> Result<(), String>;
    /// Call approved once Windows starts the elevated process, then wait for its exit.
    fn repair(&self, approved: &mut dyn FnMut()) -> RepairExit;
    /// Name it, e.g. "Kitchen (Stream To Speaker)". Returns at once.
    fn rename(&self, name: &str) -> Result<(), String>;
    /// Wait up to `max` for the last rename to be done (at quit).
    fn settle(&self, _max: std::time::Duration) {}
}

/// The defaults the app can return and the name to leave on its device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandbackPlan {
    pub our_output: Option<PcOutputId>,
    pub playback: Option<PcOutputId>,
    pub calls: Option<PcOutputId>,
    pub name: String,
}

impl HandbackPlan {
    /// A crash can happen before the driver has read the person's saved place.
    #[cfg(any(windows, test))]
    fn before_driver(outputs: &dyn Outputs) -> Self {
        let ours = outputs.our_output();
        let active: Vec<_> = outputs.list().into_iter().filter(|o| Some(&o.id) != ours.as_ref())
            .map(|o| crate::engine::place::OutputWorld { id: o.id, kind: o.kind, name: o.name }).collect();
        let default: Vec<_> = outputs.default_output().into_iter().collect();
        let pc = crate::engine::place::outputs::pc_own_output(&default, &active);
        let calls = outputs.calls_output().filter(|id| active.iter().any(|o| o.id == *id)).or_else(|| pc.clone());
        Self { our_output: ours, calls, playback: pc, name: crate::model::view::device_name_not_running(crate::model::text::Lang::English) }
    }

    fn return_roles(&self, lookup: impl FnOnce() -> Option<PcOutputId>,
        mut current: impl FnMut(Role) -> Option<PcOutputId>, mut set: impl FnMut(&PcOutputId, Role) -> Result<(), String>) {
        let Some(ours) = self.our_output.clone().or_else(lookup) else { return };
        for &role in ALL_ROLES {
            let target = if role == Role::Communications { &self.calls } else { &self.playback };
            let Some(target) = target else { continue };
            if current(role).as_ref() != Some(&ours) { continue; }
            let _ = set(target, role);
            if current(role).as_ref() == Some(&ours) {
                if let Some(pc) = &self.playback { let _ = set(pc, role); }
            }
        }
    }
}

/// What happens to the sound if the app ends without quitting.
pub trait Handback: Send + Sync {
    /// If the app dies, give Windows' output back to `output` (when this
    /// app's output is the default) and name this app's output `name`.
    /// Called again whenever either changes.
    fn prepare(&self, plan: &HandbackPlan);
    /// Return each role that still belongs to the app to its prepared output.
    fn hand_back_now(&self);
}

/// Something Windows tells the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemEvent {
    /// Windows' default output changed (the person picked another, or
    /// Windows did when headphones arrived).
    DefaultOutputChanged,
    /// An output came or went.
    OutputsChanged,
    /// The PC is about to sleep.
    Suspending,
    /// The PC woke.
    Resumed,
    /// The person is signing out or Windows is shutting down.
    SessionEnding,
    /// A media session came, went, or changed what it does
    /// ([`Players::list`] has them as they are now).
    PlayersChanged,
    /// This PC's network connections changed ([`Network::connections`]).
    NetworkChanged,
}

/// Called on Windows' thread; must return at once.
pub type EventSink = Arc<dyn Fn(SystemEvent) + Send + Sync>;

/// Where Windows' notices arrive.
pub trait SystemEvents: Send + Sync {
    fn subscribe(&self, sink: EventSink);
}

/// Everything the engine asks of Windows.
#[derive(Clone)]
pub struct Platform {
    pub outputs: Arc<dyn Outputs>,
    pub hotkeys: Arc<dyn Hotkeys>,
    pub sessions: Arc<dyn AudioSessions>,
    pub device: Arc<dyn OurDevice>,
    pub handback: Arc<dyn Handback>,
    pub events: Arc<dyn SystemEvents>,
    pub players: Arc<dyn Players>,
    pub pass: Arc<dyn PassThrough>,
    pub network: Arc<dyn Network>,
    pub desk: Arc<dyn Desk>,
}

/// The app makes its output Windows' default when a speaker is ready, and
/// gives it back. `false` leaves the choice to the person: the panel then
/// says Windows didn't switch and links to Sound settings (the fallback
/// where switching misbehaves).
pub const SWITCH_WINDOWS_OUTPUT: bool = true;

impl Platform {
    /// The PC this runs on: Windows' own outputs, sessions and notices on
    /// Windows (passive where they can't be reached), passive elsewhere.
    pub fn this_pc() -> Self {
        #[cfg(windows)]
        let p = win::platform();
        #[cfg(not(windows))]
        let p = Platform::passive();
        if SWITCH_WINDOWS_OUTPUT {
            p
        } else {
            p.without_switching()
        }
    }

    /// Knows nothing and does nothing: Windows' outputs are left to the
    /// person, as before the app could switch them.
    pub fn passive() -> Self {
        let p = Arc::new(Passive);
        Platform {
            outputs: p.clone(),
            hotkeys: p.clone(),
            sessions: p.clone(),
            device: p.clone(),
            handback: p.clone(),
            events: p.clone(),
            players: p.clone(),
            pass: p.clone(),
            network: p.clone(),
            desk: p,
        }
    }

    /// The same platform, except that it never changes Windows' default
    /// output: every switch is refused, so the engine says so and the
    /// person picks the output in Windows.
    pub fn without_switching(self) -> Self {
        Platform { outputs: Arc::new(NoSwitch(self.outputs)), ..self }
    }

    /// Whether this platform can say which output Windows plays through.
    pub fn knows_outputs(&self) -> bool {
        self.outputs.default_output().is_some()
    }
}

/// Outputs that never switch (see [`Platform::without_switching`]).
struct NoSwitch(Arc<dyn Outputs>);

impl Outputs for NoSwitch {
    fn default_for(&self, role: Role) -> Option<PcOutputId> { self.0.default_for(role) }
    fn our_output(&self) -> Option<PcOutputId> {
        self.0.our_output()
    }
    fn describe(&self, id: &PcOutputId) -> Option<OutputInfo> {
        self.0.describe(id)
    }
    fn list(&self) -> Vec<OutputInfo> {
        self.0.list()
    }
    fn set_default(&self, _: &PcOutputId, _: &[Role]) -> Result<(), String> {
        Err("Windows' output is left to the person on this PC".into())
    }
    fn volume(&self, id: &PcOutputId) -> Option<(u8, bool)> {
        self.0.volume(id)
    }
    fn set_volume(&self, id: &PcOutputId, level: u8, muted: bool) -> Result<(), String> {
        self.0.set_volume(id, level, muted)
    }
}

struct Passive;

impl Outputs for Passive {
    fn default_for(&self, _: Role) -> Option<PcOutputId> { None }
    fn our_output(&self) -> Option<PcOutputId> {
        None
    }
    fn describe(&self, _: &PcOutputId) -> Option<OutputInfo> {
        None
    }
    fn list(&self) -> Vec<OutputInfo> {
        Vec::new()
    }
    fn set_default(&self, _: &PcOutputId, _: &[Role]) -> Result<(), String> {
        Err("this platform can't switch Windows' output".into())
    }
    fn volume(&self, _: &PcOutputId) -> Option<(u8, bool)> {
        None
    }
    fn set_volume(&self, _: &PcOutputId, _: u8, _: bool) -> Result<(), String> {
        Err("this platform can't set an output's level".into())
    }
}

impl AudioSessions for Passive {
    fn sessions(&self) -> Vec<AppAudio> {
        Vec::new()
    }
    fn peak(&self, _: &PcOutputId) -> f32 {
        0.0
    }
}

impl OurDevice for Passive {
    fn turned_off(&self) -> bool { false }
    fn turn_on(&self) -> Result<(), String> { Err("No Windows audio worker".into()) }
    fn repair(&self, _: &mut dyn FnMut()) -> RepairExit { RepairExit::Failed }
    fn rename(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

impl Handback for Passive {
    fn prepare(&self, _: &HandbackPlan) {}
    fn hand_back_now(&self) {}
}

impl SystemEvents for Passive {
    fn subscribe(&self, _: EventSink) {}
}

impl Players for Passive {
    fn list(&self) -> Vec<Player> {
        Vec::new()
    }
    fn pause(&self, _: &str) -> Result<(), String> {
        Err("this platform can't reach players".into())
    }
    fn play(&self, _: &str) -> Result<(), String> {
        Err("this platform can't reach players".into())
    }
}

impl PassThrough for Passive {
    fn open(&self, _: &PcOutputId) -> Result<(), String> {
        Err("this platform can't play on the PC's own outputs".into())
    }
    fn write(&self, _: &[i16], _: u32, _: u16) {}
    fn close(&self) {}
}

/// A platform that can't tell says this PC has one connection, wired and
/// without a name: an empty list would read as "this PC is offline", and
/// every speaker that stops answering would be put down to that.
impl Network for Passive {
    fn connections(&self) -> Vec<NetConnection> {
        vec![NetConnection { id: "unknown".into(), name: None, kind: ConnKind::Wired, adapter: None, private: None }]
    }
}

impl Desk for Passive {
    fn screen_reader(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_apps_key_is_its_programs_name_in_lower_case_without_exe() {
        assert_eq!(app_key(r"C:\Users\someone\AppData\Roaming\Spotify\Spotify.exe"), "spotify");
        assert_eq!(app_key("Spotify.exe"), "spotify");
        assert_eq!(app_key("chrome.EXE"), "chrome");
        assert_eq!(app_key("/usr/bin/vlc"), "vlc");
        assert_eq!(app_key("Apple Music.exe"), "apple music", "a space in the name stays");
        assert_eq!(app_key("foobar2000"), "foobar2000");
        assert_eq!(app_key(""), "");
    }
}
