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
    refuse_switch: bool,
    sessions: Vec<AppAudio>,
    switches: Vec<(PcOutputId, Vec<Role>)>,
    names: Vec<String>,
    handback: Option<(Option<PcOutputId>, String)>,
    #[cfg(test)]
    preparations: usize,
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
        let mut state = State { outputs, off: vec![id(MONITOR)], ours_present: true, ..Default::default() };
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

    /// What the app said to do if it dies.
    pub fn handback(&self) -> Option<(Option<PcOutputId>, String)> {
        self.state.lock().unwrap().handback.clone()
    }

    /// Number of times the engine prepared the crash handback.
    #[cfg(test)]
    pub(crate) fn preparations(&self) -> usize {
        self.state.lock().unwrap().preparations
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

    fn list(&self) -> Vec<OutputInfo> {
        let st = self.state.lock().unwrap();
        st.outputs
            .iter()
            .filter(|o| !st.off.contains(&o.id) && (o.id != id(OURS) || st.ours_present))
            .cloned()
            .collect()
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
        #[cfg(test)]
        {
            self.state.lock().unwrap().preparations += 1;
        }
        self.state.lock().unwrap().handback = Some((output.cloned(), name.to_string()));
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
