//! What Windows does for the engine, behind small traits: which output
//! Windows plays through and switching it, the outputs' names and levels,
//! which apps are playing where, this app's own device, being told of
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
    /// Windows' default output (the console role), or `None` when this
    /// platform can't tell.
    fn default_output(&self) -> Option<PcOutputId>;
    /// This app's own output (its driver's endpoint), while Windows has it.
    fn our_output(&self) -> Option<PcOutputId>;
    /// Windows' names for an output; `None` once it is gone.
    fn describe(&self, id: &PcOutputId) -> Option<OutputInfo>;
    /// Every output that is switched on, this app's own among them.
    fn list(&self) -> Vec<OutputInfo>;
    /// Make `id` Windows' default output for `roles`. The caller reads the
    /// default back to know whether Windows took it.
    fn set_default(&self, id: &PcOutputId, roles: &[Role]) -> Result<(), String>;
    /// An output's level (0–100) and whether it is muted.
    fn volume(&self, id: &PcOutputId) -> Option<(u8, bool)>;
    fn set_volume(&self, id: &PcOutputId, level: u8, muted: bool) -> Result<(), String>;
}

/// An app's sound on one output.
#[derive(Clone, Debug, PartialEq)]
pub struct AppAudio {
    /// The app's name as Windows shows it ("Spotify").
    pub app: String,
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

/// This app's own output in Windows.
pub trait OurDevice: Send + Sync {
    /// Name it, e.g. "Kitchen (Stream To Speaker)".
    fn rename(&self, name: &str) -> Result<(), String>;
}

/// What happens to the sound if the app ends without quitting.
pub trait Handback: Send + Sync {
    /// If the app dies, give Windows' output back to `output` (when this
    /// app's output is the default) and name this app's output `name`.
    /// Called again whenever either changes.
    fn prepare(&self, output: Option<&PcOutputId>, name: &str);
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
    pub sessions: Arc<dyn AudioSessions>,
    pub device: Arc<dyn OurDevice>,
    pub handback: Arc<dyn Handback>,
    pub events: Arc<dyn SystemEvents>,
}

impl Platform {
    /// Knows nothing and does nothing: Windows' outputs are left to the
    /// person, as before the app could switch them.
    pub fn passive() -> Self {
        let p = Arc::new(Passive);
        Platform { outputs: p.clone(), sessions: p.clone(), device: p.clone(), handback: p.clone(), events: p }
    }

    /// Whether this platform can say which output Windows plays through.
    pub fn knows_outputs(&self) -> bool {
        self.outputs.default_output().is_some()
    }
}

struct Passive;

impl Outputs for Passive {
    fn default_output(&self) -> Option<PcOutputId> {
        None
    }
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
    fn rename(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

impl Handback for Passive {
    fn prepare(&self, _: Option<&PcOutputId>, _: &str) {}
}

impl SystemEvents for Passive {
    fn subscribe(&self, _: EventSink) {}
}
