//! Windows, played in memory for tests and demos: three outputs (the PC's
//! speakers, headphones, and this app's own), a default that a test can
//! move as the person would, apps that play, and a record of what the app
//! asked for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::{
    AppAudio, AudioSessions, EventSink, Handback, OurDevice, OutputInfo, Outputs, PcOutputId, Platform, Role,
    SystemEvent, SystemEvents,
};

/// The PC's own speakers.
pub const SPEAKERS: &str = "realtek";
/// Headphones.
pub const HEADPHONES: &str = "headphones";
/// This app's output.
pub const OURS: &str = "stream-to-speaker";

#[derive(Default)]
struct State {
    outputs: Vec<OutputInfo>,
    levels: HashMap<PcOutputId, (u8, bool)>,
    /// Per role.
    default: HashMap<Role, PcOutputId>,
    ours_present: bool,
    refuse_switch: bool,
    sessions: Vec<AppAudio>,
    switches: Vec<(PcOutputId, Vec<Role>)>,
    names: Vec<String>,
    handback: Option<(Option<PcOutputId>, String)>,
}

/// Windows in memory.
pub struct FakePlatform {
    state: Mutex<State>,
    sinks: Mutex<Vec<EventSink>>,
}

fn id(s: &str) -> PcOutputId {
    PcOutputId::new(s)
}

impl FakePlatform {
    /// The PC's speakers are Windows' default for every role.
    pub fn new() -> Arc<Self> {
        let outputs = vec![
            OutputInfo { id: id(SPEAKERS), friendly: "Speakers (Realtek Audio)".into(), name: "Speakers".into() },
            OutputInfo {
                id: id(HEADPHONES),
                friendly: "Headphones (WH-1000XM4)".into(),
                name: "Headphones".into(),
            },
            OutputInfo { id: id(OURS), friendly: "Stream To Speaker".into(), name: "Stream To Speaker".into() },
        ];
        let mut state = State { outputs, ours_present: true, ..Default::default() };
        for r in super::ALL_ROLES {
            state.default.insert(*r, id(SPEAKERS));
        }
        state.levels.insert(id(SPEAKERS), (40, false));
        state.levels.insert(id(HEADPHONES), (24, false));
        state.levels.insert(id(OURS), (100, false));
        Arc::new(FakePlatform { state: Mutex::new(state), sinks: Mutex::new(Vec::new()) })
    }

    /// This fake as the engine's platform.
    pub fn platform(self: &Arc<Self>) -> Platform {
        Platform {
            outputs: self.clone(),
            sessions: self.clone(),
            device: self.clone(),
            handback: self.clone(),
            events: self.clone(),
        }
    }

    fn fire(&self, event: SystemEvent) {
        let sinks: Vec<EventSink> = self.sinks.lock().unwrap().clone();
        for s in sinks {
            s(event.clone());
        }
    }

    /// The person (or Windows) makes `output` the default for every role.
    pub fn choose_in_windows(&self, output: &str) {
        {
            let mut st = self.state.lock().unwrap();
            for r in super::ALL_ROLES {
                st.default.insert(*r, id(output));
            }
        }
        self.fire(SystemEvent::DefaultOutputChanged);
    }

    /// Windows refuses switches from now on (or takes them again).
    pub fn refuse_switches(&self, refuse: bool) {
        self.state.lock().unwrap().refuse_switch = refuse;
    }

    /// This app's output disappears from Windows, or comes back.
    pub fn set_our_output_present(&self, present: bool) {
        self.state.lock().unwrap().ours_present = present;
        self.fire(SystemEvent::OutputsChanged);
    }

    /// `app` plays on `output` with `peak`; a peak of 0 leaves a silent
    /// session; `None` ends it.
    pub fn app_plays(&self, app: &str, output: &str, peak: Option<f32>) {
        let mut st = self.state.lock().unwrap();
        st.sessions.retain(|s| !(s.app == app && s.output == id(output)));
        if let Some(peak) = peak {
            st.sessions.push(AppAudio { app: app.into(), output: id(output), peak, system_sounds: app == "System" });
        }
    }

    /// Windows says something.
    pub fn say(&self, event: SystemEvent) {
        self.fire(event);
    }

    /// The default output for `role` now.
    pub fn default_for(&self, role: Role) -> Option<PcOutputId> {
        self.state.lock().unwrap().default.get(&role).cloned()
    }

    /// Every switch the app asked for, in order.
    pub fn switches(&self) -> Vec<(PcOutputId, Vec<Role>)> {
        self.state.lock().unwrap().switches.clone()
    }

    /// Every name given to the app's output, in order.
    pub fn names(&self) -> Vec<String> {
        self.state.lock().unwrap().names.clone()
    }

    /// What the app said to do if it dies.
    pub fn handback(&self) -> Option<(Option<PcOutputId>, String)> {
        self.state.lock().unwrap().handback.clone()
    }

    /// An output's level.
    pub fn level_of(&self, output: &str) -> Option<(u8, bool)> {
        self.state.lock().unwrap().levels.get(&id(output)).copied()
    }
}

impl Outputs for FakePlatform {
    fn default_output(&self) -> Option<PcOutputId> {
        self.state.lock().unwrap().default.get(&Role::Console).cloned()
    }

    fn our_output(&self) -> Option<PcOutputId> {
        self.state.lock().unwrap().ours_present.then(|| id(OURS))
    }

    fn describe(&self, output: &PcOutputId) -> Option<OutputInfo> {
        let st = self.state.lock().unwrap();
        if *output == id(OURS) && !st.ours_present {
            return None;
        }
        st.outputs.iter().find(|o| &o.id == output).cloned()
    }

    fn set_default(&self, output: &PcOutputId, roles: &[Role]) -> Result<(), String> {
        let changed = {
            let mut st = self.state.lock().unwrap();
            st.switches.push((output.clone(), roles.to_vec()));
            if st.refuse_switch {
                return Ok(());
            }
            if !st.outputs.iter().any(|o| &o.id == output) || (*output == id(OURS) && !st.ours_present) {
                return Err("no such output".into());
            }
            let mut changed = false;
            for r in roles {
                changed |= st.default.insert(*r, output.clone()).as_ref() != Some(output);
            }
            changed
        };
        if changed {
            self.fire(SystemEvent::DefaultOutputChanged);
        }
        Ok(())
    }

    fn volume(&self, output: &PcOutputId) -> Option<(u8, bool)> {
        self.state.lock().unwrap().levels.get(output).copied()
    }

    fn set_volume(&self, output: &PcOutputId, level: u8, muted: bool) -> Result<(), String> {
        self.state.lock().unwrap().levels.insert(output.clone(), (level, muted));
        Ok(())
    }
}

impl AudioSessions for FakePlatform {
    fn sessions(&self) -> Vec<AppAudio> {
        self.state.lock().unwrap().sessions.clone()
    }

    fn peak(&self, output: &PcOutputId) -> f32 {
        let st = self.state.lock().unwrap();
        st.sessions.iter().filter(|s| &s.output == output).map(|s| s.peak).fold(0.0, f32::max)
    }
}

impl OurDevice for FakePlatform {
    fn rename(&self, name: &str) -> Result<(), String> {
        self.state.lock().unwrap().names.push(name.to_string());
        Ok(())
    }
}

impl Handback for FakePlatform {
    fn prepare(&self, output: Option<&PcOutputId>, name: &str) {
        self.state.lock().unwrap().handback = Some((output.cloned(), name.to_string()));
    }
}

impl SystemEvents for FakePlatform {
    fn subscribe(&self, sink: EventSink) {
        self.sinks.lock().unwrap().push(sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_switches_reads_back_and_refuses_when_told() {
        let fake = FakePlatform::new();
        let p = fake.platform();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let h = heard.clone();
        p.events.subscribe(Arc::new(move |e| h.lock().unwrap().push(e)));
        assert_eq!(p.outputs.default_output(), Some(id(SPEAKERS)));
        p.outputs.set_default(&id(OURS), crate::platform::PLAYBACK_ROLES).unwrap();
        assert_eq!(p.outputs.default_output(), Some(id(OURS)));
        assert_eq!(fake.default_for(Role::Communications), Some(id(SPEAKERS)), "calls stay");
        assert_eq!(*heard.lock().unwrap(), vec![SystemEvent::DefaultOutputChanged]);
        fake.refuse_switches(true);
        p.outputs.set_default(&id(SPEAKERS), crate::platform::PLAYBACK_ROLES).unwrap();
        assert_eq!(p.outputs.default_output(), Some(id(OURS)), "refused: nothing moved");
        fake.set_our_output_present(false);
        assert_eq!(p.outputs.our_output(), None);
        assert!(p.outputs.describe(&id(OURS)).is_none());
    }

    #[test]
    fn the_passive_platform_knows_nothing() {
        let p = Platform::passive();
        assert!(!p.knows_outputs());
        assert!(p.outputs.set_default(&id(OURS), crate::platform::PLAYBACK_ROLES).is_err());
        assert!(p.sessions.sessions().is_empty());
    }
}
