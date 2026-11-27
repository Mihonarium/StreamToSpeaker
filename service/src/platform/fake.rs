//! Windows, played in memory for tests and demos: four outputs (the PC's
//! speakers, headphones, a monitor that is switched off, and this app's
//! own), a default that a test can move as the person would, apps that
//! play, players that obey or not, a network that changes, a screen reader
//! that is on or off, and a record of what the app asked for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::{
    app_key, AppAudio, AudioSessions, ConnKind, Desk, EventSink, Handback, NetConnection, Network, OurDevice,
    OutputInfo, OutputKind, Outputs, PassThrough, PcOutputId, Platform, Player, PlayerStatus, Players, Role,
    SystemEvent, SystemEvents,
};

/// The PC's own speakers.
pub const SPEAKERS: &str = "realtek";
/// Headphones.
pub const HEADPHONES: &str = "headphones";
/// A monitor's speakers, switched off in Windows.
pub const MONITOR: &str = "monitor";
/// This app's output.
pub const OURS: &str = "stream-to-speaker";
/// The network this PC is on at home.
pub const HOME_NET: &str = "home-wifi";

/// What the app asked a player to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asked {
    Pause(String),
    Play(String),
}

/// What the app did with the pass-through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Passed {
    Opened(PcOutputId),
    /// Frames written while an output was open: how many, at which rate,
    /// with how many channels.
    Wrote { frames: usize, rate: u32, channels: u16 },
    Closed,
}

#[derive(Default)]
struct State {
    outputs: Vec<OutputInfo>,
    /// Switched off in Windows.
    off: Vec<PcOutputId>,
    levels: HashMap<PcOutputId, (u8, bool)>,
    /// Per role.
    default: HashMap<Role, PcOutputId>,
    ours_present: bool,
    protected_install: bool,
    refuse_turn_on: bool,
    turn_on_calls: usize,
    repair_exit: super::RepairExit,
    repair_calls: usize,
    refuse_switch: bool,
    refuse_role: Option<Role>,
    occupied_keys: Vec<Vec<String>>,
    registered_keys: Vec<(Vec<String>, super::DelayPress)>,
    key_sink: Option<std::sync::Weak<dyn Fn(super::DelayPress) + Send + Sync>>,
    sessions: Vec<AppAudio>,
    switches: Vec<(PcOutputId, Vec<Role>)>,
    names: Vec<String>,
    handback: Option<super::HandbackPlan>,
    #[cfg(test)]
    preparations: usize,
    handbacks: usize,
    players: Vec<Player>,
    /// Players that take a pause or a play without doing it.
    deaf: Vec<String>,
    asked: Vec<Asked>,
    pass_open: Option<PcOutputId>,
    passed: Vec<Passed>,
    connections: Vec<NetConnection>,
    screen_reader: bool,
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
        let output = |key: &str, name: &str, device: &str, kind| OutputInfo {
            id: id(key),
            friendly: format!("{name} ({device})"),
            name: name.into(),
            device: device.into(),
            kind,
        };
        let outputs = vec![
            output(SPEAKERS, "Speakers", "Realtek Audio", OutputKind::Speakers),
            output(HEADPHONES, "Headphones", "WH-1000XM4", OutputKind::Headphones),
            output(MONITOR, "DELL U2720Q", "NVIDIA High Definition Audio", OutputKind::Display),
            OutputInfo {
                id: id(OURS),
                friendly: "Stream To Speaker".into(),
                name: "Stream To Speaker".into(),
                device: "Stream To Speaker".into(),
                kind: OutputKind::Other,
            },
        ];
        let mut state = State { outputs, off: vec![id(MONITOR)], ours_present: true, protected_install: true, ..Default::default() };
        for r in super::ALL_ROLES {
            state.default.insert(*r, id(SPEAKERS));
        }
        state.levels.insert(id(SPEAKERS), (40, false));
        state.levels.insert(id(HEADPHONES), (24, false));
        state.levels.insert(id(OURS), (100, false));
        state.connections =
            vec![NetConnection { id: HOME_NET.into(), name: Some("Home".into()), kind: ConnKind::WiFi }];
        Arc::new(FakePlatform { state: Mutex::new(state), sinks: Mutex::new(Vec::new()) })
    }

    /// This fake as the engine's platform.
    pub fn platform(self: &Arc<Self>) -> Platform {
        Platform {
            outputs: self.clone(),
            hotkeys: self.clone(),
            sessions: self.clone(),
            device: self.clone(),
            handback: self.clone(),
            events: self.clone(),
            players: self.clone(),
            pass: self.clone(),
            network: self.clone(),
            desk: self.clone(),
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

    /// Keys already registered by another program.
    pub fn occupy_keys(&self, keys: Vec<String>) {
        self.state.lock().unwrap().occupied_keys.push(keys);
    }

    /// Press a key combination in the foreground app.
    pub fn press_keys(&self, keys: &[String]) -> bool {
        let st = self.state.lock().unwrap();
        let Some(sink) = st.key_sink.as_ref().and_then(|s| s.upgrade()) else { return false };
        let Some(press) = st.registered_keys.iter().find(|(chord, _)| chord == keys).map(|(_, p)| *p) else { return false };
        drop(st);
        sink(press);
        true
    }

    /// Windows refuses switches from now on (or takes them again).
    pub fn refuse_switches(&self, refuse: bool) {
        self.state.lock().unwrap().refuse_switch = refuse;
    }

    #[cfg(test)]
    pub(crate) fn refuse_role(&self, role: Option<Role>) {
        self.state.lock().unwrap().refuse_role = role;
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
            let system_sounds = app == "System";
            let key = if system_sounds { String::new() } else { app_key(app) };
            st.sessions.push(AppAudio { app: app.into(), key, output: id(output), peak, system_sounds });
        }
    }

    /// `app` has a media session that does `status` (it gets one if it had
    /// none); `None` ends its session. The session's id is the app's key.
    pub fn player(&self, app: &str, status: Option<PlayerStatus>) {
        {
            let mut st = self.state.lock().unwrap();
            let key = app_key(app);
            match status {
                None => st.players.retain(|p| p.id != key),
                Some(status) => match st.players.iter_mut().find(|p| p.id == key) {
                    Some(p) => p.status = status,
                    None => st.players.push(Player {
                        id: key.clone(),
                        app: Some(key),
                        name: Some(app.to_string()),
                        status,
                        title: None,
                    }),
                },
            }
        }
        self.fire(SystemEvent::PlayersChanged);
    }

    /// A media session exactly as given (one Windows names in a way that
    /// matches no program, one with a title); it replaces the session of
    /// the same id.
    pub fn put_player(&self, player: Player) {
        {
            let mut st = self.state.lock().unwrap();
            st.players.retain(|p| p.id != player.id);
            st.players.push(player);
        }
        self.fire(SystemEvent::PlayersChanged);
    }

    /// The player of `app` takes a pause or a play without doing it from
    /// now on (or obeys again).
    pub fn player_ignores(&self, app: &str, ignores: bool) {
        let mut st = self.state.lock().unwrap();
        let key = app_key(app);
        st.deaf.retain(|k| *k != key);
        if ignores {
            st.deaf.push(key);
        }
    }

    /// What the app asked of players, in order.
    pub fn asked(&self) -> Vec<Asked> {
        self.state.lock().unwrap().asked.clone()
    }

    /// What the app did with the pass-through, in order.
    pub fn passed(&self) -> Vec<Passed> {
        self.state.lock().unwrap().passed.clone()
    }

    /// The output the pass-through plays on now.
    pub fn passing_to(&self) -> Option<PcOutputId> {
        self.state.lock().unwrap().pass_open.clone()
    }

    /// This PC's connections are these from now on (none: offline).
    pub fn connect_to(&self, connections: Vec<NetConnection>) {
        self.state.lock().unwrap().connections = connections;
        self.fire(SystemEvent::NetworkChanged);
    }

    /// A screen reader starts or ends.
    pub fn set_screen_reader(&self, on: bool) {
        self.state.lock().unwrap().screen_reader = on;
    }

    /// An output is switched on in Windows (plugged in, connected) or off.
    pub fn set_output_on(&self, output: &str, on: bool) {
        {
            let mut st = self.state.lock().unwrap();
            st.off.retain(|o| *o != id(output));
            if !on {
                st.off.push(id(output));
            }
        }
        self.fire(SystemEvent::OutputsChanged);
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

    pub fn set_protected_install(&self, protected: bool) { self.state.lock().unwrap().protected_install = protected; }
    pub fn refuse_turn_on(&self, refuse: bool) { self.state.lock().unwrap().refuse_turn_on = refuse; }
    pub fn set_repair_exit(&self, exit: super::RepairExit) { self.state.lock().unwrap().repair_exit = exit; }
    pub fn recovery_calls(&self) -> (usize, usize) {
        let st = self.state.lock().unwrap();
        (st.turn_on_calls, st.repair_calls)
    }

    /// What the app said to do if it dies.
    pub fn handback(&self) -> Option<super::HandbackPlan> {
        self.state.lock().unwrap().handback.clone()
    }

    /// Number of times the engine prepared the crash handback.
    #[cfg(test)]
    pub(crate) fn preparations(&self) -> usize {
        self.state.lock().unwrap().preparations
    }

    pub fn handbacks(&self) -> usize { self.state.lock().unwrap().handbacks }

    /// An output's level.
    pub fn level_of(&self, output: &str) -> Option<(u8, bool)> {
        self.state.lock().unwrap().levels.get(&id(output)).copied()
    }
}

impl super::Hotkeys for FakePlatform {
    fn register(&self, keys: &super::DelayKeys, sink: super::KeySink) -> super::KeyRegistration {
        let mut st = self.state.lock().unwrap();
        st.registered_keys.clear();
        st.key_sink = Some(Arc::downgrade(&sink));
        let registered = super::DelayPress::ALL.map(|press| {
            let chord = press.keys(keys);
            if st.occupied_keys.contains(&chord) { return super::KeyStatus::Taken; }
            if chord.is_empty() || st.registered_keys.iter().any(|(k, _)| *k == chord) {
                return super::KeyStatus::Rejected;
            }
            st.registered_keys.push((chord, press));
            super::KeyStatus::Registered
        });
        super::KeyRegistration::new(registered, sink)
    }
}

impl Outputs for FakePlatform {
    fn default_for(&self, role: Role) -> Option<PcOutputId> { FakePlatform::default_for(self, role) }

    fn our_output(&self) -> Option<PcOutputId> {
        let st = self.state.lock().unwrap();
        (st.ours_present && !st.off.contains(&id(OURS))).then(|| id(OURS))
    }

    fn describe(&self, output: &PcOutputId) -> Option<OutputInfo> {
        let st = self.state.lock().unwrap();
        if *output == id(OURS) && !st.ours_present {
            return None;
        }
        st.outputs.iter().find(|o| &o.id == output).cloned()
    }

    fn current_friendly(&self, id: &PcOutputId) -> Option<String> {
        let info = self.describe(id)?;
        if self.our_output().as_ref() == Some(id) {
            if let Some(name) = self.state.lock().unwrap().names.last() { return Some(name.clone()); }
        }
        Some(info.friendly)
    }

    fn list(&self) -> Vec<OutputInfo> {
        let st = self.state.lock().unwrap();
        st.outputs
            .iter()
            .filter(|o| !st.off.contains(&o.id) && (o.id != id(OURS) || st.ours_present))
            .cloned()
            .collect()
    }

    fn set_default(&self, output: &PcOutputId, roles: &[Role]) -> Result<(), String> {
        let (changed, result) = {
            let mut st = self.state.lock().unwrap();
            st.switches.push((output.clone(), roles.to_vec()));
            if st.refuse_switch {
                return Ok(());
            }
            if st.off.contains(output) || !st.outputs.iter().any(|o| &o.id == output) || (*output == id(OURS) && !st.ours_present) {
                return Err("no such output".into());
            }
            let mut changed = false;
            let mut result = Ok(());
            for r in roles {
                if st.refuse_role == Some(*r) {
                    result = Err("Windows refused".into());
                    break;
                }
                changed |= st.default.insert(*r, output.clone()).as_ref() != Some(output);
            }
            (changed, result)
        };
        if changed {
            self.fire(SystemEvent::DefaultOutputChanged);
        }
        result
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
    fn can_repair(&self) -> bool { self.state.lock().unwrap().protected_install }
    fn turned_off(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.ours_present && st.off.contains(&id(OURS))
    }

    fn turn_on(&self) -> Result<(), String> {
        {
            let mut st = self.state.lock().unwrap();
            st.turn_on_calls += 1;
            if st.refuse_turn_on || !st.ours_present { return Err("Windows refused".into()); }
        }
        self.set_output_on(OURS, true);
        Ok(())
    }

    fn repair(&self, approved: &mut dyn FnMut()) -> super::RepairExit {
        let exit = {
            let mut st = self.state.lock().unwrap();
            if !st.protected_install { return super::RepairExit::Failed; }
            st.repair_calls += 1;
            st.repair_exit
        };
        if exit != super::RepairExit::Cancelled { approved(); }
        exit
    }

    fn rename(&self, name: &str) -> Result<(), String> {
        self.state.lock().unwrap().names.push(name.to_string());
        Ok(())
    }
}

impl Handback for FakePlatform {
    fn hand_back_now(&self) {
        let plan = {
            let mut state = self.state.lock().unwrap();
            state.handbacks += 1;
            state.handback.clone()
        };
        if let Some(plan) = plan {
            plan.return_roles(|| Some(id(OURS)), |role| self.default_for(role), |target, role| self.set_default(target, &[role]));
        }
    }

    fn prepare(&self, plan: &super::HandbackPlan) {
        #[cfg(test)]
        {
            self.state.lock().unwrap().preparations += 1;
        }
        self.state.lock().unwrap().handback = Some(plan.clone());
    }
}

impl SystemEvents for FakePlatform {
    fn subscribe(&self, sink: EventSink) {
        self.sinks.lock().unwrap().push(sink);
    }
}

impl FakePlatform {
    /// A pause or a play asked of `player`: recorded, and done unless the
    /// player ignores it.
    fn ask(&self, player: &str, asked: Asked, becomes: PlayerStatus) -> Result<(), String> {
        let changed = {
            let mut st = self.state.lock().unwrap();
            if !st.players.iter().any(|p| p.id == player) {
                return Err("no such player".into());
            }
            st.asked.push(asked);
            if st.deaf.iter().any(|k| k == player) {
                false
            } else {
                let p = st.players.iter_mut().find(|p| p.id == player).expect("checked above");
                let changed = p.status != becomes;
                p.status = becomes;
                changed
            }
        };
        if changed {
            self.fire(SystemEvent::PlayersChanged);
        }
        Ok(())
    }
}

impl Players for FakePlatform {
    fn list(&self) -> Vec<Player> {
        self.state.lock().unwrap().players.clone()
    }

    fn pause(&self, player: &str) -> Result<(), String> {
        self.ask(player, Asked::Pause(player.to_string()), PlayerStatus::Paused)
    }

    fn play(&self, player: &str) -> Result<(), String> {
        self.ask(player, Asked::Play(player.to_string()), PlayerStatus::Playing)
    }
}

impl PassThrough for FakePlatform {
    fn open(&self, output: &PcOutputId) -> Result<(), String> {
        let mut st = self.state.lock().unwrap();
        if st.off.contains(output) || !st.outputs.iter().any(|o| &o.id == output) || *output == id(OURS) {
            return Err("no such output of this PC".into());
        }
        st.pass_open = Some(output.clone());
        st.passed.push(Passed::Opened(output.clone()));
        Ok(())
    }

    fn write(&self, frames: &[i16], rate: u32, channels: u16) {
        let mut st = self.state.lock().unwrap();
        if st.pass_open.is_some() {
            let frames = frames.len() / usize::from(channels.max(1));
            st.passed.push(Passed::Wrote { frames, rate, channels });
        }
    }

    fn close(&self) {
        let mut st = self.state.lock().unwrap();
        if st.pass_open.take().is_some() {
            st.passed.push(Passed::Closed);
        }
    }
}

impl Network for FakePlatform {
    fn connections(&self) -> Vec<NetConnection> {
        self.state.lock().unwrap().connections.clone()
    }
}

impl Desk for FakePlatform {
    fn screen_reader(&self) -> bool {
        self.state.lock().unwrap().screen_reader
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_calls_require_an_installed_copy_and_report_approval_separately() {
        use crate::platform::RepairExit;
        let fake = FakePlatform::new();
        let mut approvals = 0;
        fake.set_protected_install(false);
        assert_eq!(fake.repair(&mut || approvals += 1), RepairExit::Failed);
        fake.set_protected_install(true);
        fake.set_repair_exit(RepairExit::Cancelled);
        assert_eq!(fake.repair(&mut || approvals += 1), RepairExit::Cancelled);
        fake.set_repair_exit(RepairExit::Failed);
        assert_eq!(fake.repair(&mut || approvals += 1), RepairExit::Failed);
        assert_eq!(approvals, 1);
        assert_eq!(fake.recovery_calls(), (0, 2));
        assert!(fake.switches().is_empty());
    }

    #[test]
    fn loopback_opens_the_current_app_name_or_follows_the_default_without_passing_it_back() {
        use crate::audio_source::{LocalPlayback, LoopbackEndpoint};
        let fake = FakePlatform::new();
        for name in ["Stream To Speaker", "Kitchen (Stream To Speaker)"] {
            fake.rename(name).unwrap();
            let opened = LoopbackEndpoint::new(fake.clone());
            assert_eq!(opened.name().unwrap().as_deref(), Some(name));
            assert_eq!(opened.local_playback(), LocalPlayback::Any);
            assert_eq!(LocalPlayback::for_named_output(&*fake, name), LocalPlayback::Except(id(OURS)));
            fake.choose_in_windows(HEADPHONES);
            assert!(!opened.changed());
        }
        let app = LoopbackEndpoint::new(fake.clone());
        fake.set_our_output_present(false);
        assert!(app.changed());
        let fallback = LoopbackEndpoint::new(fake.clone());
        assert_eq!(fallback.name().unwrap(), None);
        assert_eq!(fallback.local_playback(), LocalPlayback::WindowsDefault);
        fake.choose_in_windows(SPEAKERS);
        assert!(fallback.changed());
        let fallback = LoopbackEndpoint::new(fake.clone());
        fake.set_our_output_present(true);
        assert!(fallback.changed());
    }

    #[test]
    fn before_the_first_driver_plan_calls_can_return_to_an_active_pc_output() {
        let windows = FakePlatform::new();
        windows.choose_in_windows(OURS);
        windows.set_default(&id(HEADPHONES), &[Role::Communications]).unwrap();
        let plan = super::super::HandbackPlan::before_driver(&*windows);
        assert_eq!(plan.our_output, Some(id(OURS)));
        assert_eq!(plan.playback, Some(id(SPEAKERS)));
        assert_eq!(plan.calls, Some(id(HEADPHONES)));
    }

    #[test]
    fn handback_compares_each_role_with_the_live_id_before_using_the_lookup() {
        let windows = FakePlatform::new();
        let mut plan = super::super::HandbackPlan { our_output: Some(id(HEADPHONES)), playback: Some(id(SPEAKERS)),
            calls: Some(id(SPEAKERS)), name: String::new() };
        windows.choose_in_windows(OURS);
        windows.set_default(&id(HEADPHONES), &[Role::Communications]).unwrap();
        windows.prepare(&plan);
        windows.hand_back_now();
        assert_eq!(windows.calls_output(), Some(id(SPEAKERS)));
        assert_eq!(windows.default_output(), Some(id(OURS)));
        plan.our_output = None;
        windows.prepare(&plan);
        windows.hand_back_now();
        assert_eq!(windows.default_output(), Some(id(SPEAKERS)));
    }

    #[test]
    fn handback_retries_the_playback_target_after_a_silent_refusal() {
        let windows = FakePlatform::new();
        windows.set_default(&id(OURS), &[Role::Communications]).unwrap();
        windows.prepare(&super::super::HandbackPlan { our_output: Some(id(OURS)), playback: Some(id(SPEAKERS)),
            calls: Some(id(HEADPHONES)), name: String::new() });
        windows.refuse_switches(true);
        windows.hand_back_now();
        assert_eq!(windows.switches(), [OURS, HEADPHONES, SPEAKERS].map(|o| (id(o), vec![Role::Communications])));
    }

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
    fn without_switching_every_switch_is_refused_and_the_rest_still_works() {
        let fake = FakePlatform::new();
        let p = fake.platform().without_switching();
        assert!(p.outputs.set_default(&id(OURS), crate::platform::PLAYBACK_ROLES).is_err());
        assert_eq!(p.outputs.default_output(), Some(id(SPEAKERS)));
        assert!(fake.switches().is_empty(), "Windows was never asked");
        assert_eq!(p.outputs.list().len(), 3, "speakers, headphones and ours; not the monitor that is off");
        p.outputs.set_volume(&id(HEADPHONES), 30, false).unwrap();
        assert_eq!(fake.level_of(HEADPHONES), Some((30, false)));
    }

    #[test]
    fn the_passive_platform_knows_nothing() {
        let p = Platform::passive();
        assert!(!p.knows_outputs());
        assert!(p.outputs.set_default(&id(OURS), crate::platform::PLAYBACK_ROLES).is_err());
        assert!(p.sessions.sessions().is_empty());
        assert!(p.players.list().is_empty());
        assert!(p.players.pause("spotify").is_err());
        assert!(p.pass.open(&id(SPEAKERS)).is_err());
        assert!(!p.desk.screen_reader());
        assert_eq!(p.network.connections().len(), 1, "it can't tell, so it doesn't say this PC is offline");
    }

    #[test]
    fn players_pause_and_play_through_the_seam() {
        let fake = FakePlatform::new();
        let p = fake.platform();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let h = heard.clone();
        p.events.subscribe(Arc::new(move |e| h.lock().unwrap().push(e)));
        fake.player("Spotify.exe", Some(PlayerStatus::Playing));
        let list = p.players.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].app.as_deref(), Some("spotify"), "the key its sound carries too");
        fake.app_plays("Spotify.exe", OURS, Some(0.4));
        assert_eq!(p.sessions.sessions()[0].key, "spotify");

        p.players.pause("spotify").unwrap();
        assert_eq!(p.players.list()[0].status, PlayerStatus::Paused);
        p.players.play("spotify").unwrap();
        assert_eq!(p.players.list()[0].status, PlayerStatus::Playing);
        assert_eq!(fake.asked(), vec![Asked::Pause("spotify".into()), Asked::Play("spotify".into())]);
        assert_eq!(heard.lock().unwrap().iter().filter(|e| **e == SystemEvent::PlayersChanged).count(), 3);

        fake.player_ignores("Spotify.exe", true);
        p.players.pause("spotify").unwrap();
        assert_eq!(p.players.list()[0].status, PlayerStatus::Playing, "asked, and it went on playing");
        assert!(p.players.pause("vlc").is_err(), "no such player");
        fake.player("Spotify.exe", None);
        assert!(p.players.list().is_empty());
    }

    #[test]
    fn the_pass_through_plays_on_one_of_the_pcs_own_outputs() {
        let fake = FakePlatform::new();
        let p = fake.platform();
        p.pass.write(&[0; 8], 48_000, 2);
        assert!(fake.passed().is_empty(), "nothing is open: nothing is played");
        assert!(p.pass.open(&id(OURS)).is_err(), "never the app's own device");
        assert!(p.pass.open(&id(MONITOR)).is_err(), "an output that is switched off");
        p.pass.open(&id(SPEAKERS)).unwrap();
        p.pass.write(&[0; 8], 48_000, 2);
        p.pass.open(&id(HEADPHONES)).unwrap();
        assert_eq!(fake.passing_to(), Some(id(HEADPHONES)));
        p.pass.close();
        p.pass.close();
        assert_eq!(
            fake.passed(),
            vec![
                Passed::Opened(id(SPEAKERS)),
                Passed::Wrote { frames: 4, rate: 48_000, channels: 2 },
                Passed::Opened(id(HEADPHONES)),
                Passed::Closed,
            ]
        );
    }

    #[test]
    fn network_changes_and_outputs_that_come_and_go_are_told() {
        let fake = FakePlatform::new();
        let p = fake.platform();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let h = heard.clone();
        p.events.subscribe(Arc::new(move |e| h.lock().unwrap().push(e)));
        assert_eq!(p.network.connections()[0].id, HOME_NET);
        fake.connect_to(Vec::new());
        assert!(p.network.connections().is_empty(), "offline");
        fake.set_output_on(HEADPHONES, false);
        assert!(!p.outputs.list().iter().any(|o| o.id == id(HEADPHONES)));
        fake.set_output_on(HEADPHONES, true);
        assert!(p.outputs.list().iter().any(|o| o.id == id(HEADPHONES)));
        assert_eq!(
            *heard.lock().unwrap(),
            vec![SystemEvent::NetworkChanged, SystemEvent::OutputsChanged, SystemEvent::OutputsChanged]
        );
        fake.set_screen_reader(true);
        assert!(p.desk.screen_reader());
    }
}
