//! A stand-in for the engine around the core: it keeps the world, answers
//! Windows' and the players' requests as they usually go, feeds levels of
//! the apps that sound, and records what the core asked for.

pub use super::super::numbers::{AfterFailedStart, AfterHeadphones};
pub use super::super::*;
pub use crate::model::command::{Command, FirstRunAction, Surface};
pub use crate::model::facts::OutputKind;
pub use crate::model::situation::*;
pub use crate::model::state::{Place, Secret};
pub use crate::model::text::LocalTime;
pub use sound::Level;

pub const K: &str = "kitchen";
pub const B: &str = "bedroom";
pub const OFFICE: &str = "office";

pub fn id(s: &str) -> SpeakerId {
    SpeakerId::new(s)
}

pub fn out(s: &str) -> PcOutputId {
    PcOutputId::new(s)
}

pub fn home() -> NetId {
    NetId("home".into())
}

pub const SPOTIFY: &str = "Spotify";
pub const CHROME: &str = "Chrome";
pub const GAME: &str = "Game";

/// A PC with its speakers, on the home Wi-Fi, with Kitchen and Bedroom
/// (Sonos) and Office (AirPlay) found there.
pub fn world() -> World {
    let on_home = |s: SpeakerWorld| SpeakerWorld { network: Some(home()), ..s };
    World {
        speakers: vec![
            on_home(SpeakerWorld::found(K)),
            on_home(SpeakerWorld::found(B)),
            on_home(SpeakerWorld {
                ways: vec![WayChoice::AirPlaySteady, WayChoice::AirPlayShort],
                shows_doing: false,
                level_unreadable: true,
                ..SpeakerWorld::found(OFFICE)
            }),
        ],
        outputs: vec![OutputWorld { id: out("speakers"), kind: OutputKind::Speakers, name: "Speakers".into() }],
        default_output: Some(OutputRef::Pc(out("speakers"))),
        multimedia_output: Some(OutputRef::Pc(out("speakers"))),
        our_device: true,
        players: Vec::new(),
        connections: vec![Connection { net: home(), name: Some("HomeNet".into()), kind: ConnKind::WiFi }],
        settings: SettingsWorld::default(),
        screen_reader: false,
        panel_open: false,
        music_assistant_playing: false,
    }
}

impl World {
    pub fn set_playback(&mut self, to: OutputRef) {
        self.multimedia_output = Some(to.clone());
        self.default_output = Some(to);
    }
}

/// An app that sounds: a player's sound follows Windows' default output
/// while it plays; other sound follows it always, or stays on one output.
#[derive(Clone, Debug)]
pub struct Source {
    pub app: AppKey,
    pub player: Option<PlayerId>,
    pub output: Option<OutputRef>,
}

pub struct Rig {
    pub core: Core,
    pub w: World,
    pub now: Clock,
    /// What the core asked for since the last [`Rig::take`].
    pub out: Vec<Do>,
    /// Connections asked for and not yet ended or superseded.
    pub jobs: Vec<Job>,
    pub sources: Vec<Source>,
    /// Answer Windows' and the players' requests (as they usually go).
    pub obliging: bool,
}

/// 21:00:00 on the wall clock.
pub fn start_clock() -> Clock {
    Clock { mono_ms: 1_000_000, wall: WallTime(1_800_000_000_000 - 1_800_000_000_000 % 60_000), local: LocalTime::new(21, 0) }
}

impl Rig {
    /// The app started on this PC, first run over, past the minute after
    /// the start.
    pub fn new() -> Rig {
        Rig::with(Kept { first_run_done: true, ..Kept::default() }, world())
    }

    pub fn with(kept: Kept, w: World) -> Rig {
        Rig::starting(kept, w, StartKind::Normal, true)
    }

    pub fn starting(kept: Kept, w: World, kind: StartKind, past_grace: bool) -> Rig {
        Rig::starting_at(start_clock(), kept, w, kind, past_grace)
    }

    /// The app starts again at the time this rig has reached, with what
    /// it kept.
    pub fn restart(&self, w: World, kind: StartKind) -> Rig {
        Rig::starting_at(self.now.later(5_000), self.core.kept(), w, kind, false)
    }

    pub fn starting_at(now: Clock, kept: Kept, w: World, kind: StartKind, past_grace: bool) -> Rig {
        let core = Core::new(kept, now);
        let mut rig = Rig { core, w, now, out: Vec::new(), jobs: Vec::new(), sources: Vec::new(), obliging: true };
        rig.event(Event::Started(kind));
        if past_grace {
            rig.wait(61_000);
        }
        rig
    }

    /// Kitchen is the place, nothing playing.
    pub fn on_kitchen() -> Rig {
        let mut rig = Rig::new();
        rig.switch_to(K);
        rig.take();
        rig
    }

    /// Kitchen is the place and Spotify plays there.
    pub fn playing_on_kitchen() -> Rig {
        let mut rig = Rig::on_kitchen();
        rig.play(SPOTIFY);
        rig.wait(4_000);
        assert_eq!(rig.sit().doing, Doing::Playing, "set-up: Spotify plays on Kitchen");
        rig.take();
        rig
    }

    fn handle(&mut self, dos: Vec<Do>) {
        let mut follow = Vec::new();
        for d in &dos {
            match d {
                Do::Connect(j) => self.jobs.push(j.clone()),
                Do::Supersede(gen) => self.jobs.retain(|j| j.gen != *gen),
                Do::Windows(t) | Do::Silence { then: Some(t), .. } if self.obliging => {
                    let to = switch::win_ref(t);
                    if !self.w.playback_on(&to) {
                        self.w.set_playback(to);
                        follow.push(Event::Output(OutputEvent::DefaultChanged { at_ms: self.now.mono_ms }));
                    }
                }
                Do::Pause(ids) if self.obliging => {
                    for i in ids {
                        follow.extend(self.set_player(i, PlayerStatus::Paused));
                    }
                }
                Do::Play(ids) if self.obliging => {
                    for i in ids {
                        follow.extend(self.set_player(i, PlayerStatus::Playing));
                    }
                }
                _ => {}
            }
        }
        self.out.extend(dos);
        for e in follow {
            self.event(e);
        }
    }

    fn set_player(&mut self, i: &PlayerId, status: PlayerStatus) -> Option<Event> {
        let p = self.w.players.iter_mut().find(|p| &p.id == i)?;
        if p.status == status {
            return None;
        }
        p.status = status;
        Some(Event::Player(PlayerEvent::Status { id: i.clone(), status }))
    }

    pub fn cmd(&mut self, c: Command) -> Result<Effect, Refusal> {
        self.cmd_from(Surface::Panel, c)
    }

    pub fn cmd_from(&mut self, from: Surface, c: Command) -> Result<Effect, Refusal> {
        let (result, dos) = self.core.command(from, &c, &self.w, self.now);
        self.handle(dos);
        result
    }

    pub fn event(&mut self, e: Event) {
        let dos = self.core.event(e, &self.w, self.now);
        self.handle(dos);
    }

    pub fn tick(&mut self) {
        let dos = self.core.tick(&self.w, self.now);
        self.handle(dos);
    }

    /// The levels of what sounds now.
    pub fn levels(&self) -> Vec<Level> {
        self.sources
            .iter()
            .filter(|s| {
                s.player.as_ref().is_none_or(|p| self.w.players.iter().any(|x| &x.id == p && x.status == PlayerStatus::Playing))
            })
            .filter_map(|s| {
                let output = s.output.clone().or_else(|| self.w.default_output.clone())?;
                Some(Level { app: s.app.clone(), output, peak: 0.1, system: false })
            })
            .collect()
    }

    /// Time passes, a sample of the levels every 250 ms.
    pub fn wait(&mut self, ms: u64) {
        let mut left = ms;
        while left > 0 {
            let step = left.min(250);
            self.now = self.now.later(step);
            let levels = self.levels();
            self.event(Event::Sound(levels));
            left -= step;
        }
    }

    /// Long waits: a sample each second while something sounds, each
    /// minute otherwise, and at each moment the core asks to be woken.
    pub fn wait_long(&mut self, ms: u64) {
        let mut left = ms;
        while left > 0 {
            let mut step = if self.levels().is_empty() { 60_000 } else { 1_000 }.min(left);
            if let Some(due) = self.core.next_due() {
                if due > self.now.mono_ms {
                    step = step.min(due - self.now.mono_ms);
                }
            }
            if step <= 250 {
                self.wait(step);
            } else if self.levels().is_empty() {
                self.now = self.now.later(step);
                self.event(Event::Sound(Vec::new()));
            } else {
                // Four samples a second keep the sound one run.
                self.wait(step);
            }
            left -= step;
        }
    }

    pub fn take(&mut self) -> Vec<Do> {
        std::mem::take(&mut self.out)
    }

    pub fn sit(&self) -> Situation {
        self.core.situation(&self.w, self.now)
    }

    /// The live connection with this purpose, the newest.
    pub fn job(&self, purpose: Purpose) -> Job {
        self.jobs.iter().rev().find(|j| j.purpose == purpose).cloned().unwrap_or_else(|| {
            panic!("no live {purpose:?} connection; live: {:?}; asked: {:?}", self.jobs, self.out)
        })
    }

    pub fn has_job(&self, purpose: Purpose) -> bool {
        self.jobs.iter().any(|j| j.purpose == purpose)
    }

    /// The live connection with this purpose ends.
    pub fn end(&mut self, purpose: Purpose, ended: Ended) {
        let j = self.job(purpose);
        self.jobs.retain(|x| x.gen != j.gen);
        self.event(Event::Connected { gen: j.gen, ended });
    }

    /// A person's switch from the panel that gets ready.
    pub fn switch_to(&mut self, s: &str) {
        self.cmd(Command::Switch { place: Place::Speaker(id(s)), way: None }).unwrap();
        self.end(Purpose::Switch, Ended::Ready);
        assert_eq!(self.sit().spot, Spot::Speaker(id(s)), "set-up: the switch to {s} took effect");
    }

    pub fn speaker(&mut self, s: &str, signal: SpeakerSignal) {
        self.event(Event::Speaker { id: id(s), signal });
    }

    /// A player starts (a person presses play in it).
    pub fn play(&mut self, app: &str) {
        let pid = PlayerId::new(app);
        if !self.w.players.iter().any(|p| p.id == pid) {
            self.w.players.push(PlayerWorld {
                id: pid.clone(),
                app: AppKey::new(app),
                name: Some(app.to_string()),
                status: PlayerStatus::Paused,
            });
            self.sources.push(Source { app: AppKey::new(app), player: Some(pid.clone()), output: None });
        }
        if let Some(e) = self.set_player(&pid, PlayerStatus::Playing) {
            self.event(e);
        }
    }

    /// A person pauses a player.
    pub fn pause_by_hand(&mut self, app: &str) {
        if let Some(e) = self.set_player(&PlayerId::new(app), PlayerStatus::Paused) {
            self.event(e);
        }
    }

    pub fn status(&self, app: &str) -> PlayerStatus {
        self.w.players.iter().find(|p| p.id == PlayerId::new(app)).map(|p| p.status).expect("a player")
    }

    /// Sound without a player (a call, a game) on Windows' default output.
    pub fn sound(&mut self, app: &str) {
        self.sources.push(Source { app: AppKey::new(app), player: None, output: None });
    }

    pub fn silence(&mut self, app: &str) {
        self.sources.retain(|s| s.app.as_str() != app);
    }

    /// Headphones are plugged in and Windows switches to them.
    pub fn plug_headphones(&mut self, windows_switches: bool) {
        self.w.outputs.push(OutputWorld { id: out("headphones"), kind: OutputKind::Headphones, name: "Headphones".into() });
        let t = self.now.mono_ms;
        self.event(Event::Output(OutputEvent::Added {
            id: out("headphones"),
            kind: OutputKind::Headphones,
            name: "Headphones".into(),
            at_ms: t,
        }));
        if windows_switches {
            self.now = self.now.later(50);
            self.w.set_playback(OutputRef::Pc(out("headphones")));
            self.event(Event::Output(OutputEvent::DefaultChanged { at_ms: self.now.mono_ms }));
        }
    }

    /// The headphones are pulled out; Windows lands on the speakers first
    /// and says so before it says the headphones went.
    pub fn unplug_headphones(&mut self) {
        self.w.outputs.retain(|o| o.id != out("headphones"));
        if self.w.default_output == Some(OutputRef::Pc(out("headphones"))) {
            self.w.set_playback(OutputRef::Pc(out("speakers")));
            self.event(Event::Output(OutputEvent::DefaultChanged { at_ms: self.now.mono_ms }));
        }
        self.now = self.now.later(30);
        self.event(Event::Output(OutputEvent::Removed { id: out("headphones"), at_ms: self.now.mono_ms }));
    }

    /// Windows' default changed by a person in Windows' list.
    pub fn choose_in_windows(&mut self, to: OutputRef) {
        self.w.set_playback(to);
        self.event(Event::Output(OutputEvent::DefaultChanged { at_ms: self.now.mono_ms }));
        self.wait(2_500);
    }

    pub fn open_panel(&mut self) {
        self.w.panel_open = true;
        self.cmd(Command::PanelShown(true)).unwrap();
    }

    pub fn close_panel(&mut self) {
        self.w.panel_open = false;
        self.cmd(Command::PanelShown(false)).unwrap();
    }
}

// ----- what the core asked for

pub fn connects(dos: &[Do], purpose: Purpose) -> bool {
    dos.iter().any(|d| matches!(d, Do::Connect(j) if j.purpose == purpose))
}

pub fn windows_to(dos: &[Do], target: &WinTarget) -> bool {
    dos.iter().any(|d| matches!(d, Do::Windows(t) | Do::Silence { then: Some(t), .. } if t == target))
}

pub fn to_ours(dos: &[Do]) -> bool {
    windows_to(dos, &WinTarget::OurDevice)
}

pub fn to_speakers(dos: &[Do]) -> bool {
    windows_to(dos, &WinTarget::Output(out("speakers")))
}

pub fn paused(dos: &[Do], app: &str) -> bool {
    dos.iter().any(|d| matches!(d, Do::Pause(ids) if ids.contains(&PlayerId::new(app))))
}

pub fn started(dos: &[Do], app: &str) -> bool {
    dos.iter().any(|d| matches!(d, Do::Play(ids) if ids.contains(&PlayerId::new(app))))
}

pub fn silenced(dos: &[Do], s: &str) -> bool {
    dos.iter().any(|d| matches!(d, Do::Silence { speaker, .. } if *speaker == id(s)))
}

/// The app asked no player for anything: it never pauses or starts one by
/// itself.
pub fn asks_no_player(dos: &[Do]) -> bool {
    !dos.iter().any(|d| matches!(d, Do::Pause(_) | Do::Play(_)))
}

/// What shows by the clock (a confirmation apart).
pub fn popup(rig: &Rig) -> Option<Popup> {
    rig.sit().popups.popup.map(|p| p.what)
}

/// The note the panel shows.
pub fn note(rig: &Rig) -> Option<Note> {
    rig.sit().note
}

pub fn cause(rig: &Rig) -> Option<Cause> {
    rig.sit().note.map(|n| n.cause)
}

/// The note's one button.
pub fn button(rig: &Rig) -> Option<NoteButton> {
    rig.sit().note.and_then(|n| n.button())
}

pub fn row(rig: &Rig, s: &str) -> RowState {
    rig.sit().speaker(&id(s)).map(|r| r.state).expect("a row")
}

/// One row of a table: a name that says what it proves, the moment, what
/// happens, and what must hold after it. `then` may go on with the story
/// where what must hold includes what comes next.
pub struct Row {
    pub name: &'static str,
    pub given: fn() -> Rig,
    pub when: fn(&mut Rig),
    pub then: fn(&mut Rig, &[Do]),
}

pub fn run(rows: &[Row]) {
    for row in rows {
        let mut rig = (row.given)();
        rig.take();
        (row.when)(&mut rig);
        let dos = rig.take();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (row.then)(&mut rig, &dos)));
        if let Err(e) = result {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            panic!("row \"{}\": {}\n  asked: {:?}\n  situation: {:?}", row.name, msg, dos, rig.sit());
        }
    }
}
