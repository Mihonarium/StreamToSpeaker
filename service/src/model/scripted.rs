//! A scripted engine: the port in memory, with named scenarios, one for
//! each state and each way the panel looks. It reacts to commands the way
//! the engine does (choosing a place walks through connecting to playing; a
//! delay button changes the numbers and the line under the buttons; the
//! volume moves), on a virtual clock that only moves when told to, so
//! previews, demos and tests are deterministic.
//!
//! ```
//! use stream_to_speaker::model::{scripted::{Scenario, ScriptedEngine}, Command, Place, Port, Surface};
//! let engine = ScriptedEngine::new(Scenario::OnThisPc);
//! let kitchen = engine.speaker_id("Kitchen");
//! engine.send(Surface::Panel, Command::Play(Place::Speaker(kitchen)));
//! engine.settle();
//! assert_eq!(engine.snapshot().top.sentence, "Playing on Kitchen");
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::command::{
    ticket, Command, DelayOutcome, DelayStep, Effect, FirstRunAct, NoticeKind, Outcome, Refusal, Reply,
    Surface, Ticket, VolumeOwner,
};
use super::delay::press;
use super::facts::*;
use super::port::{Port, PortCore, Watch, Watcher};
use super::rules::{self, volume_owner, within_limit};
use super::snapshot::{build, Snapshot};
use super::state::*;
use super::text::{Lang, LocalTime};

/// Every named scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scenario {
    OnThisPc,
    Connecting,
    ConnectingFromPc,
    Playing,
    Muted,
    Ready,
    ReadyElsewhere,
    BusyQuestion,
    NotTaking,
    NotResponding,
    StaySilent,
    FellBack,
    SpeakerBack,
    Taken,
    NeedsPairing,
    LookingForSpeaker,
    CantFind,
    WindowsSwitched,
    ChooseSpeaker,
    WindowsRefused,
    MaPlaying,
    MaWaiting,
    MaAway,
    MaBack,
    OutputMissing,
    SentApps,
    NoAppSent,
    NothingFound,
    CrowdedNetwork,
    RowCatalogue,
    ApplyingDelay,
    DelayOwn,
    DelayRemoving,
    DelayAtLimit,
    DelayAdding,
    DelayGoingBack,
    DelayReconnectPending,
    DelayNotAccepted,
    DelaySteadyRemove,
    DelaySwitchedToShort,
    DelayFixedOnSteady,
    DelaySendspin,
    DelayDroppedOut,
    DelayDropouts,
    NoticeRestart,
    NoticeAdapterOff,
    NoticeRefused,
    NoticeLab,
    NoticeUpdate,
    NoticeUpdated,
    NoticeDonation,
    FirstRunPick,
    FirstRunHear,
    FirstRunFindIt,
    /// Kitchen reached two ways (Sonos and AirPlay), Office likewise.
    Twins,
    /// This PC with three outputs, the list under it closed.
    PcOutputsClosed,
    /// The same, open.
    PcOutputsOpen,
}

impl Scenario {
    pub const ALL: &'static [Scenario] = &[
        Scenario::OnThisPc,
        Scenario::Connecting,
        Scenario::ConnectingFromPc,
        Scenario::Playing,
        Scenario::Muted,
        Scenario::Ready,
        Scenario::ReadyElsewhere,
        Scenario::BusyQuestion,
        Scenario::NotTaking,
        Scenario::NotResponding,
        Scenario::StaySilent,
        Scenario::FellBack,
        Scenario::SpeakerBack,
        Scenario::Taken,
        Scenario::NeedsPairing,
        Scenario::LookingForSpeaker,
        Scenario::CantFind,
        Scenario::WindowsSwitched,
        Scenario::ChooseSpeaker,
        Scenario::WindowsRefused,
        Scenario::MaPlaying,
        Scenario::MaWaiting,
        Scenario::MaAway,
        Scenario::MaBack,
        Scenario::OutputMissing,
        Scenario::SentApps,
        Scenario::NoAppSent,
        Scenario::NothingFound,
        Scenario::CrowdedNetwork,
        Scenario::RowCatalogue,
        Scenario::ApplyingDelay,
        Scenario::DelayOwn,
        Scenario::DelayRemoving,
        Scenario::DelayAtLimit,
        Scenario::DelayAdding,
        Scenario::DelayGoingBack,
        Scenario::DelayReconnectPending,
        Scenario::DelayNotAccepted,
        Scenario::DelaySteadyRemove,
        Scenario::DelaySwitchedToShort,
        Scenario::DelayFixedOnSteady,
        Scenario::DelaySendspin,
        Scenario::DelayDroppedOut,
        Scenario::DelayDropouts,
        Scenario::NoticeRestart,
        Scenario::NoticeAdapterOff,
        Scenario::NoticeRefused,
        Scenario::NoticeLab,
        Scenario::NoticeUpdate,
        Scenario::NoticeUpdated,
        Scenario::NoticeDonation,
        Scenario::FirstRunPick,
        Scenario::FirstRunHear,
        Scenario::FirstRunFindIt,
        Scenario::Twins,
        Scenario::PcOutputsClosed,
        Scenario::PcOutputsOpen,
    ];

    /// The scenario's name, for preview tools and demos ("fell-back").
    pub fn name(self) -> &'static str {
        use Scenario::*;
        match self {
            OnThisPc => "on-this-pc",
            Connecting => "connecting",
            ConnectingFromPc => "connecting-from-pc",
            Playing => "playing",
            Muted => "muted",
            Ready => "ready",
            ReadyElsewhere => "ready-elsewhere",
            BusyQuestion => "busy-question",
            NotTaking => "not-taking",
            NotResponding => "not-responding",
            StaySilent => "stay-silent",
            FellBack => "fell-back",
            SpeakerBack => "speaker-back",
            Taken => "taken",
            NeedsPairing => "needs-pairing",
            LookingForSpeaker => "looking-for-speaker",
            CantFind => "cant-find",
            WindowsSwitched => "windows-switched",
            ChooseSpeaker => "choose-speaker",
            WindowsRefused => "windows-refused",
            MaPlaying => "music-assistant-playing",
            MaWaiting => "music-assistant-waiting",
            MaAway => "music-assistant-away",
            MaBack => "music-assistant-back",
            OutputMissing => "output-missing",
            SentApps => "sent-apps",
            NoAppSent => "no-app-sent",
            NothingFound => "nothing-found",
            CrowdedNetwork => "crowded-network",
            RowCatalogue => "row-catalogue",
            ApplyingDelay => "applying-delay",
            DelayOwn => "delay-own",
            DelayRemoving => "delay-removing",
            DelayAtLimit => "delay-at-limit",
            DelayAdding => "delay-adding",
            DelayGoingBack => "delay-going-back",
            DelayReconnectPending => "delay-reconnect-pending",
            DelayNotAccepted => "delay-not-accepted",
            DelaySteadyRemove => "delay-steady-remove",
            DelaySwitchedToShort => "delay-switched-to-short",
            DelayFixedOnSteady => "delay-fixed-on-steady",
            DelaySendspin => "delay-sendspin",
            DelayDroppedOut => "delay-dropped-out",
            DelayDropouts => "delay-dropouts",
            NoticeRestart => "notice-restart",
            NoticeAdapterOff => "notice-adapter-off",
            NoticeRefused => "notice-refused",
            NoticeLab => "notice-lab",
            NoticeUpdate => "notice-update",
            NoticeUpdated => "notice-updated",
            NoticeDonation => "notice-donation",
            FirstRunPick => "first-run-pick",
            FirstRunHear => "first-run-hear",
            FirstRunFindIt => "first-run-find-it",
            Twins => "twins",
            PcOutputsClosed => "pc-outputs-closed",
            PcOutputsOpen => "pc-outputs-open",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.iter().copied().find(|s| s.name() == name)
    }
}

/// The speakers' names in a scenario, to try long names and other scripts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Names {
    Usual,
    /// Long names, to see where text wraps or is cut.
    Long,
    /// Chinese names, to see the fallback fonts.
    Chinese,
}

/// How one of the scenario's speakers behaves when it plays.
#[derive(Clone, Debug)]
struct Profile {
    applies: Applies,
    range: DelayRange,
    /// AirPlay: (Steady, Short) delays and ranges.
    choice: Option<(ChoiceFacts, DelayRange, DelayRange)>,
    /// AirPlay 2: on Steady nothing can be changed yet.
    fixed_on_steady: bool,
    level: Level,
    needs_code: bool,
}

#[derive(Clone, Copy, Debug)]
enum Event {
    Stage(u64, ConnectStage),
    Arrive(u64),
    Fail(u64),
    MaStarts(u64),
    DelayTick,
    ReconnectStart(u64),
    ReconnectEnd(u64),
}

struct Sim {
    facts: Facts,
    now_ms: u64,
    queue: VecDeque<(Surface, Command, Reply)>,
    timers: Vec<(u64, u64, Event)>,
    next_timer: u64,
    received: Vec<(Surface, Command)>,
    profiles: HashMap<SpeakerId, Profile>,
    /// What plays on this PC: decides Playing or Ready on arrival.
    apps: Apps,
    connect_gen: u64,
    ma_gen: u64,
    delay_gen: u64,
    reconnect_from: Option<i32>,
    reconnect_again: bool,
    ticking: bool,
    /// What each AirPlay speaker keeps on its other delay choice.
    other_choice: HashMap<SpeakerId, i32>,
}

/// The port in memory. Commands queue; [`ScriptedEngine::advance`] and
/// [`ScriptedEngine::settle`] handle them on the calling thread and move the
/// virtual clock.
pub struct ScriptedEngine {
    core: PortCore,
    sim: Mutex<Sim>,
}

impl ScriptedEngine {
    pub fn new(scenario: Scenario) -> Self {
        Self::with_names(scenario, Names::Usual)
    }

    pub fn with_names(scenario: Scenario, names: Names) -> Self {
        let (facts, profiles, apps) = scenario_facts(scenario, names);
        let core = PortCore::new(build(&facts, 1));
        if facts.sending_sound {
            core.set_level(0.6);
        }
        let mut sim = Sim {
            facts,
            now_ms: 0,
            queue: VecDeque::new(),
            timers: Vec::new(),
            next_timer: 0,
            received: Vec::new(),
            profiles,
            apps,
            connect_gen: 0,
            ma_gen: 0,
            delay_gen: 0,
            reconnect_from: None,
            reconnect_again: false,
            ticking: false,
            other_choice: HashMap::new(),
        };
        sim.resume();
        ScriptedEngine { core, sim: Mutex::new(sim) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Sim> {
        self.sim.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The id of the speaker named `name` in this scenario (with the usual
    /// names), for tests and demos.
    pub fn speaker_id(&self, name: &str) -> SpeakerId {
        let sim = self.lock();
        sim.facts
            .speakers
            .iter()
            .find(|s| s.name == name || s.id.as_str() == name.to_lowercase())
            .map(|s| s.id.clone())
            .unwrap_or_else(|| SpeakerId::new(name))
    }

    /// The facts behind the current snapshot.
    pub fn facts(&self) -> Facts {
        self.lock().facts.clone()
    }

    /// Every command received, in order.
    pub fn received(&self) -> Vec<(Surface, Command)> {
        self.lock().received.clone()
    }

    /// The virtual clock.
    pub fn now_ms(&self) -> u64 {
        self.lock().now_ms
    }

    /// Handles the queued commands, then moves the clock by `ms`, running
    /// what falls due.
    pub fn advance(&self, ms: u64) {
        let mut sim = self.lock();
        let mut changed = sim.handle_queue(&self.core);
        let until = sim.now_ms + ms;
        changed |= sim.run_timers(until);
        sim.now_ms = until;
        if changed {
            self.publish(&sim);
        }
    }

    /// Handles the queued commands and runs every timer to the end (a
    /// connection arrives, a delay change finishes applying).
    pub fn settle(&self) {
        for _ in 0..10_000 {
            let next = {
                let sim = self.lock();
                if !sim.queue.is_empty() {
                    Some(sim.now_ms)
                } else {
                    sim.timers.iter().map(|t| t.0).min()
                }
            };
            match next {
                Some(at) => {
                    let now = self.now_ms();
                    self.advance(at.saturating_sub(now));
                }
                None => return,
            }
        }
    }

    fn publish(&self, sim: &Sim) {
        self.core.set_level(if sim.facts.sending_sound { 0.6 } else { 0.0 });
        self.core.publish(build(&sim.facts, self.core.next_version()));
    }

    /// Replaces the facts outright (for a test's own situation).
    pub fn set_facts(&self, facts: Facts) {
        let mut sim = self.lock();
        sim.facts = facts;
        self.publish(&sim);
    }
}

impl Port for ScriptedEngine {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.core.snapshot()
    }

    fn watch(&self, watcher: Watcher) -> Watch {
        self.core.watch(watcher)
    }

    fn send(&self, from: Surface, command: Command) -> Ticket {
        let (reply, ticket) = ticket();
        self.lock().queue.push_back((from, command, reply));
        ticket
    }

    fn level(&self) -> f32 {
        self.core.level()
    }
}

/// Moves a scripted engine's clock in real time on a thread of its own,
/// for interactive demos. Stops when dropped.
pub struct RealTime {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RealTime {
    pub fn start(engine: Arc<ScriptedEngine>, tick: Duration) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(tick);
                engine.advance(tick.as_millis() as u64);
            }
        });
        RealTime { stop, thread: Some(thread) }
    }
}

impl Drop for RealTime {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Reacting to commands.

const STAGE_MS: u64 = 600;
const CODE_MS: u64 = 2_400;
const MA_START_MS: u64 = 1_500;
/// About 9 ms of delay per second of playback (4 frames per 10 ms packet).
const DELAY_TICK_MS: u64 = 111;
const RECONNECT_MS: u64 = 3_000;

impl Sim {
    fn at(&mut self, delay_ms: u64, event: Event) {
        self.next_timer += 1;
        let at = self.now_ms + delay_ms;
        self.timers.push((at, self.next_timer, event));
    }

    /// Schedules what an in-flight scenario still has to do.
    fn resume(&mut self) {
        if let NowState::Connecting { target, .. } = &self.facts.state {
            let needs_code = self.profiles.get(target).is_some_and(|p| p.needs_code);
            self.schedule_connect(needs_code);
        }
        if let Some(d) = self.facts.delay.clone() {
            match d.progress {
                Progress::Live { .. } => self.start_ticking(),
                Progress::ReconnectPending => self.at(rules::DELAY_RECONNECT_AFTER_MS, Event::ReconnectStart(0)),
                Progress::Reconnecting => self.at(RECONNECT_MS, Event::ReconnectEnd(0)),
                Progress::Settled => {}
            }
            if let NowState::ApplyingDelay { from_ms, .. } = self.facts.state {
                self.reconnect_from = Some(from_ms);
            }
            if let Some(DelayNote::ChoiceSwitched { other_kept_ms }) = d.note {
                self.other_choice.insert(d.speaker.clone(), other_kept_ms);
            }
        }
    }

    fn handle_queue(&mut self, core: &PortCore) -> bool {
        let mut changed = false;
        while let Some((from, command, reply)) = self.queue.pop_front() {
            self.received.push((from, command.clone()));
            let result = self.handle(command);
            changed = true;
            let snapshot = core.publish(build(&self.facts, core.next_version()));
            reply.send(Outcome { result, snapshot });
        }
        changed
    }

    fn run_timers(&mut self, until: u64) -> bool {
        let mut changed = false;
        loop {
            self.timers.sort_by_key(|t| (t.0, t.1));
            if self.timers.first().is_none_or(|t| t.0 > until) {
                return changed;
            }
            let (at, _, event) = self.timers.remove(0);
            self.now_ms = at;
            self.fire(event);
            changed = true;
        }
    }

    fn speaker(&mut self, id: &SpeakerId) -> Option<&mut SpeakerFacts> {
        self.facts.speakers.iter_mut().find(|s| &s.id == id)
    }

    fn sound_at(&self) -> SoundAt {
        match self.facts.state.current_place() {
            Some(Place::Speaker(id)) => SoundAt::Speaker(id),
            Some(Place::MusicAssistant) => SoundAt::MusicAssistant,
            _ => SoundAt::ThisPc,
        }
    }

    fn handle(&mut self, command: Command) -> Result<Effect, Refusal> {
        match command {
            Command::Play(Place::ThisPc) | Command::StayOnThisPc | Command::Quit => {
                self.to_this_pc();
                Ok(Effect::Done)
            }
            Command::Release { .. } => {
                self.to_this_pc();
                Ok(Effect::Done)
            }
            Command::PlayOnPcOutput(output) => {
                match self.facts.pc.outputs.iter().find(|o| o.id == output) {
                    Some(o) => self.facts.pc.output = o.clone(),
                    None => return Ok(Effect::Done),
                }
                self.to_this_pc();
                Ok(Effect::Done)
            }
            Command::ShowPcOutputs(open) => {
                self.facts.pc.outputs_open = open;
                Ok(Effect::Done)
            }
            Command::Play(Place::Speaker(id)) => self.play(id, false),
            Command::PlayTakingOver(id) => self.play(id, true),
            Command::Play(Place::MusicAssistant) => {
                self.leave_speaker();
                self.facts.state = NowState::MaWaiting;
                self.facts.volume.place = Some(Level::at(70));
                self.ma_gen += 1;
                let gen = self.ma_gen;
                self.at(MA_START_MS, Event::MaStarts(gen));
                Ok(Effect::Done)
            }
            Command::CancelConnect => {
                if let NowState::Connecting { from, .. } = self.facts.state.clone() {
                    self.connect_gen += 1;
                    self.back_to(from);
                }
                Ok(Effect::Done)
            }
            Command::DismissQuestion(id) => {
                if let Some(s) = self.speaker(&id) {
                    s.question = None;
                }
                Ok(Effect::Done)
            }
            Command::SetVolume { owner, level } => {
                self.set_level(&owner, |l| l.value = within_limit(level.min(100), l.limit));
                Ok(Effect::Done)
            }
            Command::SetMuted { owner, muted } => {
                self.set_level(&owner, |l| l.muted = muted);
                Ok(Effect::Done)
            }
            Command::ChangeDelay { speaker, step } => self.change_delay(speaker, step),
            Command::UseDelayChoice { speaker, choice } => {
                self.use_choice(&speaker, choice);
                Ok(Effect::Done)
            }
            Command::Reconnect(id) => {
                if self.facts.state.current_place() == Some(Place::Speaker(id.clone())) {
                    let needs_code = self.profiles.get(&id).is_some_and(|p| p.needs_code);
                    self.facts.state = NowState::Connecting {
                        target: id.clone(),
                        from: SoundAt::Speaker(id),
                        stage: ConnectStage::Asking,
                    };
                    self.schedule_connect(needs_code);
                }
                Ok(Effect::Done)
            }
            Command::HideFromList(id) => {
                if let Some(s) = self.speaker(&id) {
                    s.hidden = true;
                }
                Ok(Effect::Done)
            }
            Command::AllowDevice { .. } => {
                self.facts.notices.refused_in_group = None;
                Ok(Effect::Done)
            }
            Command::TurnOffLab => {
                self.facts.notices.lab_on = false;
                Ok(Effect::Done)
            }
            Command::DismissNotice(kind) => {
                let n = &mut self.facts.notices;
                match kind {
                    NoticeKind::Updated => n.updated_to = None,
                    NoticeKind::Donation => n.donation_due = false,
                    _ => {}
                }
                Ok(Effect::Done)
            }
            Command::PanelShown(shown) => {
                if shown {
                    self.facts.seen = true;
                }
                Ok(Effect::Done)
            }
            Command::FirstRun(act) => {
                self.first_run(act);
                Ok(Effect::Done)
            }
            Command::PlayTestSound(_) | Command::Open(_) | Command::SettingsShown(_) => Ok(Effect::Done),
        }
    }

    fn first_run(&mut self, act: FirstRunAct) {
        let step = self.facts.first_run.clone();
        self.facts.first_run = match (act, step) {
            (FirstRunAct::Heard, Some(FirstRun::Hear { speaker })) => {
                Some(FirstRun::FindIt { speaker, start_at_sign_in: true })
            }
            (FirstRunAct::StartAtSignIn(on), Some(FirstRun::FindIt { speaker, .. })) => {
                Some(FirstRun::FindIt { speaker, start_at_sign_in: on })
            }
            (FirstRunAct::PlayChimeAgain, step) => step,
            (FirstRunAct::NotNow | FirstRunAct::Close | FirstRunAct::Done, _) => None,
            (_, step) => step,
        };
    }

    fn set_level(&mut self, owner: &VolumeOwner, f: impl FnOnce(&mut Level)) {
        if volume_owner(&self.facts.state, &self.facts.pc.output.id).as_ref() != Some(owner) {
            return;
        }
        match owner {
            VolumeOwner::PcOutput(_) => {
                if let Some(l) = self.facts.volume.pc.as_mut() {
                    f(l)
                }
            }
            _ => {
                if let Some(l) = self.facts.volume.place.as_mut() {
                    f(l)
                }
            }
        }
    }

    /// Back to where the sound was before a connection that didn't finish.
    fn back_to(&mut self, from: SoundAt) {
        match from {
            SoundAt::ThisPc => self.to_this_pc(),
            SoundAt::Speaker(id) => self.arrive(&id),
            SoundAt::MusicAssistant => {
                self.facts.state = NowState::MaPlaying { apps: self.apps.clone(), room: None };
                self.facts.sending_sound = !self.apps.0.is_empty();
            }
        }
    }

    fn leave_speaker(&mut self) {
        self.connect_gen += 1;
        self.facts.delay = None;
        self.facts.volume.place = None;
        self.facts.sending_sound = false;
        for s in &mut self.facts.speakers {
            s.question = None;
        }
    }

    fn to_this_pc(&mut self) {
        self.leave_speaker();
        self.facts.state = NowState::OnThisPc;
    }

    fn schedule_connect(&mut self, needs_code: bool) {
        self.connect_gen += 1;
        let gen = self.connect_gen;
        self.at(STAGE_MS, Event::Stage(gen, ConnectStage::Asking));
        if needs_code {
            self.at(2 * STAGE_MS, Event::Stage(gen, ConnectStage::Pairing));
            self.at(2 * STAGE_MS + CODE_MS, Event::Stage(gen, ConnectStage::Waiting));
            self.at(3 * STAGE_MS + CODE_MS, Event::Arrive(gen));
        } else {
            self.at(2 * STAGE_MS, Event::Stage(gen, ConnectStage::Waiting));
            self.at(3 * STAGE_MS, Event::Arrive(gen));
        }
    }

    fn play(&mut self, id: SpeakerId, take_over: bool) -> Result<Effect, Refusal> {
        let Some(s) = self.speaker(&id) else {
            return Err(Refusal::UnknownPlace { name: id.as_str().to_string() });
        };
        s.failure = None;
        if take_over {
            s.busy = false;
            s.question = None;
        }
        let (busy, presence) = (s.busy, s.presence);
        for other in &mut self.facts.speakers {
            if other.id != id {
                other.question = None;
            }
        }
        let state = &self.facts.state;
        if state.current_place() == Some(Place::Speaker(id.clone()))
            && !matches!(state, NowState::NotResponding { .. })
        {
            return Ok(Effect::Done);
        }
        if busy {
            if let Some(s) = self.speaker(&id) {
                s.question = Some(TakeOver { other_pc: None });
            }
            return Ok(Effect::Done);
        }
        let from = match &self.facts.state {
            NowState::Connecting { from, .. } => from.clone(),
            _ => self.sound_at(),
        };
        self.facts.state = NowState::Connecting { target: id.clone(), from, stage: ConnectStage::Finding };
        if presence != Presence::Found {
            self.connect_gen += 1;
            let gen = self.connect_gen;
            self.at(3 * STAGE_MS, Event::Fail(gen));
        } else {
            let needs_code = self.profiles.get(&id).is_some_and(|p| p.needs_code);
            self.schedule_connect(needs_code);
        }
        Ok(Effect::Done)
    }

    fn fire(&mut self, event: Event) {
        match event {
            Event::Stage(gen, stage) => {
                if gen == self.connect_gen {
                    if let NowState::Connecting { stage: s, .. } = &mut self.facts.state {
                        *s = stage;
                    }
                }
            }
            Event::Arrive(gen) => {
                if gen != self.connect_gen {
                    return;
                }
                if let NowState::Connecting { target, .. } = self.facts.state.clone() {
                    self.arrive(&target);
                }
            }
            Event::Fail(gen) => {
                if gen != self.connect_gen {
                    return;
                }
                if let NowState::Connecting { target, from, .. } = self.facts.state.clone() {
                    self.back_to(from);
                    if let Some(s) = self.speaker(&target) {
                        s.failure = Some(ConnectFailure::NoAnswer);
                    }
                }
            }
            Event::MaStarts(gen) => {
                if gen == self.ma_gen && self.facts.state == NowState::MaWaiting {
                    self.facts.state = NowState::MaPlaying { apps: self.apps.clone(), room: None };
                    self.facts.sending_sound = !self.apps.0.is_empty();
                }
            }
            Event::DelayTick => {
                self.ticking = false;
                if let Some(d) = self.facts.delay.as_mut() {
                    if let Progress::Live { to_go_ms } = d.progress {
                        let next = to_go_ms - to_go_ms.signum();
                        d.progress = if next == 0 { Progress::Settled } else { Progress::Live { to_go_ms: next } };
                        if next != 0 {
                            self.start_ticking();
                        }
                    }
                }
            }
            Event::ReconnectStart(gen) => {
                if gen != self.delay_gen {
                    return;
                }
                let change = self.current_change();
                if let Some(d) = self.facts.delay.as_mut() {
                    d.progress = Progress::Reconnecting;
                }
                if let NowState::Playing { speaker, .. } = &self.facts.state {
                    self.facts.state = NowState::ApplyingDelay {
                        speaker: speaker.clone(),
                        from_ms: self.reconnect_from.unwrap_or(change),
                        to_ms: change,
                    };
                    self.facts.sending_sound = false;
                }
                self.reconnect_from = None;
                self.at(RECONNECT_MS, Event::ReconnectEnd(gen));
            }
            Event::ReconnectEnd(gen) => {
                if gen != self.delay_gen {
                    return;
                }
                if let NowState::ApplyingDelay { speaker, .. } = &self.facts.state {
                    self.facts.state = NowState::Playing { speaker: speaker.clone(), apps: self.apps.clone() };
                    self.facts.sending_sound = true;
                }
                if let Some(d) = self.facts.delay.as_mut() {
                    d.progress = Progress::Settled;
                }
                if std::mem::take(&mut self.reconnect_again) {
                    self.pending_reconnect();
                }
            }
        }
    }

    fn start_ticking(&mut self) {
        if !self.ticking {
            self.ticking = true;
            self.at(DELAY_TICK_MS, Event::DelayTick);
        }
    }

    fn current_change(&self) -> i32 {
        self.facts
            .delay
            .as_ref()
            .and_then(|d| self.facts.speakers.iter().find(|s| s.id == d.speaker))
            .and_then(|s| s.delay)
            .map(|d| d.change_ms)
            .unwrap_or(0)
    }

    fn pending_reconnect(&mut self) {
        self.delay_gen += 1;
        let gen = self.delay_gen;
        if let Some(d) = self.facts.delay.as_mut() {
            d.progress = Progress::ReconnectPending;
        }
        self.at(rules::DELAY_RECONNECT_AFTER_MS, Event::ReconnectStart(gen));
    }

    /// The connection is up: play or be ready, with the speaker's level and
    /// delay change.
    fn arrive(&mut self, id: &SpeakerId) {
        let profile = self.profiles.get(id).cloned();
        let name = self.speaker(id).map(|s| {
            s.used_here = true;
            s.name.clone()
        });
        self.facts.remembered = Remembered::Speaker { name: name.unwrap_or_default() };
        if self.facts.first_run == Some(FirstRun::Pick) {
            self.facts.first_run = Some(FirstRun::Hear { speaker: id.clone() });
        }
        self.facts.state = if self.apps.0.is_empty() {
            NowState::Ready { speaker: id.clone() }
        } else {
            NowState::Playing { speaker: id.clone(), apps: self.apps.clone() }
        };
        self.facts.sending_sound = !self.apps.0.is_empty();
        if let Some(p) = profile {
            self.facts.volume.place = Some(p.level);
            self.facts.delay = Some(control_facts(id, &p));
        }
    }

    fn change_delay(&mut self, speaker: SpeakerId, step: DelayStep) -> Result<Effect, Refusal> {
        let shown = self.facts.state.shows_delay() && self.facts.state.speaker() == Some(&speaker);
        let Some(control) = self.facts.delay.clone().filter(|d| d.speaker == speaker && shown) else {
            return Err(match self.facts.state {
                NowState::MaPlaying { .. } | NowState::MaWaiting => Refusal::MusicAssistantDelay,
                _ => Refusal::NoDelayHere,
            });
        };
        if control.applies == Applies::Fixed {
            return Err(Refusal::FixedOnSteady { speaker });
        }
        let before = self.current_change();
        let p = press(control.range, before, step);
        if let Some(s) = self.speaker(&speaker) {
            if let Some(d) = s.delay.as_mut() {
                d.change_ms = p.after_ms;
            }
        }
        if p.after_ms != before {
            self.facts.delay_hint_retired = true;
        }
        let on_steady = control.choice.map(|c| c.current) == Some(DelayChoice::Steady);
        let note = if matches!(step, DelayStep::Remove(_)) && on_steady {
            Some(DelayNote::SteadyRemovePressed)
        } else {
            match control.note {
                Some(DelayNote::Dropouts { .. }) if p.after_ms == before => control.note,
                _ => None,
            }
        };
        let delta = p.after_ms - before;
        if let Some(d) = self.facts.delay.as_mut() {
            d.note = note;
            match control.applies {
                Applies::Live => {
                    let to_go = match d.progress {
                        Progress::Live { to_go_ms } => to_go_ms,
                        _ => 0,
                    } + delta;
                    d.progress = if to_go == 0 { Progress::Settled } else { Progress::Live { to_go_ms: to_go } };
                }
                Applies::Reconnect | Applies::Fixed => {}
            }
        }
        if delta != 0 {
            match control.applies {
                Applies::Live => self.start_ticking(),
                Applies::Reconnect => {
                    if self.reconnect_from.is_none() {
                        self.reconnect_from = Some(before);
                    }
                    if control.progress == Progress::Reconnecting {
                        self.reconnect_again = true;
                    } else {
                        self.pending_reconnect();
                    }
                }
                Applies::Fixed => {}
            }
        }
        Ok(Effect::Delay(DelayOutcome {
            speaker,
            step,
            before_ms: before,
            after_ms: p.after_ms,
            stopped_at_limit: p.stopped_at_limit,
        }))
    }

    fn use_choice(&mut self, id: &SpeakerId, choice: DelayChoice) {
        let Some(profile) = self.profiles.get(id).cloned() else { return };
        let Some((mut facts_choice, steady_range, short_range)) = profile.choice else { return };
        let old = self.speaker(id).and_then(|s| s.delay).map(|d| d.change_ms).unwrap_or(0);
        let kept = self.other_choice.insert(id.clone(), old).unwrap_or(0);
        facts_choice.current = choice;
        let (own, range) = match choice {
            DelayChoice::Steady => (facts_choice.steady_ms, steady_range),
            DelayChoice::Short => (facts_choice.short_ms, short_range),
        };
        if let Some(s) = self.speaker(id) {
            s.delay = Some(SpeakerDelay { own_ms: own, measured: false, change_ms: kept });
        }
        let mut p = profile.clone();
        p.choice = Some((facts_choice, steady_range, short_range));
        p.range = range;
        p.applies = if choice == DelayChoice::Steady && p.fixed_on_steady {
            Applies::Fixed
        } else {
            Applies::Reconnect
        };
        self.profiles.insert(id.clone(), p.clone());
        if self.facts.delay.as_ref().is_some_and(|d| &d.speaker == id) {
            let mut control = control_facts(id, &p);
            control.note = Some(DelayNote::ChoiceSwitched { other_kept_ms: old });
            self.facts.delay = Some(control);
            if p.applies == Applies::Reconnect {
                self.reconnect_from = Some(kept);
                self.pending_reconnect();
            }
        }
    }
}

fn control_facts(id: &SpeakerId, p: &Profile) -> DelayFacts {
    DelayFacts {
        speaker: id.clone(),
        range: p.range,
        applies: p.applies,
        choice: p.choice.map(|c| c.0),
        progress: Progress::Settled,
        note: None,
    }
}

// ---------------------------------------------------------------------------
// The household the scenarios share.

fn id(s: &str) -> SpeakerId {
    SpeakerId::new(s)
}

fn output(id: &str, name: &str, device: &str, kind: OutputKind) -> PcOutput {
    PcOutput {
        id: PcOutputId::new(id),
        friendly: format!("{name} ({device})"),
        name: name.to_string(),
        device: device.to_string(),
        kind,
    }
}

fn speaker(key: &str, name: &str, kind: SpeakerKind, address: &str, own_ms: u32) -> SpeakerFacts {
    SpeakerFacts {
        id: id(key),
        name: name.to_string(),
        kind,
        address: address.to_string(),
        delay: Some(SpeakerDelay { own_ms, measured: false, change_ms: 0 }),
        presence: Presence::Found,
        used_here: true,
        hidden: false,
        busy: false,
        failure: None,
        question: None,
        twin: None,
        last_used: None,
    }
}

const SONOS: DelayRange = DelayRange { max_removed_ms: 200, max_added_ms: 1000 };
const AIRPLAY_STEADY: DelayRange = DelayRange { max_removed_ms: 1000, max_added_ms: 1000 };
const AIRPLAY_SHORT: DelayRange = DelayRange { max_removed_ms: 280, max_added_ms: 699 };

fn airplay_profile(level: u8, fixed_on_steady: bool, needs_code: bool) -> Profile {
    let choice = ChoiceFacts { current: DelayChoice::Steady, steady_ms: 2000, short_ms: 300 };
    Profile {
        applies: if fixed_on_steady { Applies::Fixed } else { Applies::Reconnect },
        range: AIRPLAY_STEADY,
        choice: Some((choice, AIRPLAY_STEADY, AIRPLAY_SHORT)),
        fixed_on_steady,
        level: Level::at(level),
        needs_code,
    }
}

fn household(names: Names) -> (Facts, HashMap<SpeakerId, Profile>) {
    let (kitchen, living, bedroom, office, garage, den, desk) = match names {
        Names::Usual => (
            "Kitchen",
            "Living Room + Dining Room",
            "Bedroom",
            "Office",
            "Garage",
            "Den TV",
            "Desk Speaker",
        ),
        Names::Long => (
            "Kitchen and Breakfast Room by the Garden Window",
            "Living Room + Dining Room + Library + Conservatory",
            "Bedroom Upstairs at the End of the Hall",
            "Office",
            "Garage Workshop Ceiling Speakers",
            "Den TV",
            "Desk Speaker",
        ),
        Names::Chinese => ("厨房", "客厅 + 餐厅", "卧室", "书房", "车库", "影音室电视", "书桌音箱"),
    };
    let mut speakers = vec![
        speaker("kitchen", kitchen, SpeakerKind::Sonos, "192.168.1.40", 200),
        speaker("living", living, SpeakerKind::SonosGroup, "192.168.1.41", 200),
        speaker("bedroom", bedroom, SpeakerKind::HomePodPair, "192.168.1.42", 200),
        speaker("office", office, SpeakerKind::AirPlay, "192.168.1.43", 2000),
        speaker("garage", garage, SpeakerKind::AirPlay, "192.168.1.44", 2000),
        speaker("den", den, SpeakerKind::AppleTv, "192.168.1.45", 2000),
        speaker("desk", desk, SpeakerKind::Sendspin, "192.168.1.46", 500),
    ];
    speakers[0].delay = Some(SpeakerDelay { own_ms: 200, measured: false, change_ms: -50 });
    speakers[4].presence = Presence::NotFound;
    // Den TV and the desk speaker join only the scenarios that show them.
    speakers[5].hidden = true;
    speakers[6].hidden = true;

    let mut profiles = HashMap::new();
    let sonos = |level| Profile {
        applies: Applies::Live,
        range: SONOS,
        choice: None,
        fixed_on_steady: false,
        level: Level::at(level),
        needs_code: false,
    };
    profiles.insert(id("kitchen"), sonos(32));
    profiles.insert(id("living"), sonos(18));
    profiles.insert(
        id("bedroom"),
        Profile {
            applies: Applies::Reconnect,
            range: DelayRange { max_removed_ms: 180, max_added_ms: 1800 },
            choice: None,
            fixed_on_steady: false,
            level: Level::at(20),
            needs_code: false,
        },
    );
    profiles.insert(id("office"), airplay_profile(30, false, false));
    profiles.insert(id("garage"), airplay_profile(30, false, false));
    profiles.insert(id("den"), airplay_profile(30, true, true));
    profiles.insert(
        id("desk"),
        Profile {
            applies: Applies::Reconnect,
            range: DelayRange { max_removed_ms: 0, max_added_ms: 800 },
            choice: None,
            fixed_on_steady: false,
            level: Level::at(40),
            needs_code: false,
        },
    );

    let facts = Facts {
        lang: Lang::English,
        state: NowState::OnThisPc,
        pc: {
            let speakers = output("realtek", "Speakers", "Realtek Audio", OutputKind::Speakers);
            PcFacts { outputs: vec![speakers.clone()], output: speakers, outputs_open: false }
        },
        speakers,
        music_assistant: Some(MaFacts { connected: true, last_seen: None }),
        search: SearchFacts::default(),
        volume: VolumeFacts { pc: Some(Level::at(40)), place: None },
        delay: None,
        delay_steps: DelaySteps::default(),
        delay_hint_retired: true,
        mode: Mode::Everything,
        remembered: Remembered::Speaker { name: kitchen.to_string() },
        seen: true,
        sending_sound: false,
        notices: NoticeFacts::default(),
        notification: None,
        first_run: None,
    };
    (facts, profiles)
}

fn t(h: u8, m: u8) -> LocalTime {
    LocalTime::new(h, m)
}

/// The facts, speaker behaviour and playing apps of a scenario.
fn scenario_facts(scenario: Scenario, names: Names) -> (Facts, HashMap<SpeakerId, Profile>, Apps) {
    use Scenario::*;
    let (mut f, mut profiles) = household(names);
    let mut apps = Apps::of(&["Spotify"]);
    let k = id("kitchen");
    let office = id("office");

    // Puts the sound on a speaker, as a finished connection would.
    let play_on = |f: &mut Facts, profiles: &HashMap<SpeakerId, Profile>, sid: &SpeakerId, apps: &Apps| {
        let p = &profiles[sid];
        f.state = if apps.0.is_empty() {
            NowState::Ready { speaker: sid.clone() }
        } else {
            NowState::Playing { speaker: sid.clone(), apps: apps.clone() }
        };
        f.sending_sound = !apps.0.is_empty();
        f.volume.place = Some(p.level);
        f.delay = Some(control_facts(sid, p));
        let name = f.speakers.iter().find(|s| &s.id == sid).map(|s| s.name.clone()).unwrap_or_default();
        f.remembered = Remembered::Speaker { name };
    };
    let show = |f: &mut Facts, key: &str| {
        if let Some(s) = f.speakers.iter_mut().find(|s| s.id.as_str() == key) {
            s.hidden = false;
        }
    };
    let set_change = |f: &mut Facts, key: &str, change: i32| {
        if let Some(s) = f.speakers.iter_mut().find(|s| s.id.as_str() == key) {
            if let Some(d) = s.delay.as_mut() {
                d.change_ms = change;
            }
        }
    };
    let note = |f: &mut Facts, progress: Progress, note: Option<DelayNote>| {
        if let Some(d) = f.delay.as_mut() {
            d.progress = progress;
            d.note = note;
        }
    };

    match scenario {
        OnThisPc => {}
        Connecting => {
            play_on(&mut f, &profiles, &k, &apps);
            f.delay = None;
            f.state = NowState::Connecting {
                target: id("bedroom"),
                from: SoundAt::Speaker(k.clone()),
                stage: ConnectStage::Asking,
            };
        }
        ConnectingFromPc => {
            f.state = NowState::Connecting { target: k.clone(), from: SoundAt::ThisPc, stage: ConnectStage::Finding };
        }
        Playing => play_on(&mut f, &profiles, &k, &apps),
        Muted => {
            play_on(&mut f, &profiles, &k, &apps);
            if let Some(l) = f.volume.place.as_mut() {
                l.muted = true;
            }
        }
        Ready => {
            apps = Apps::default();
            play_on(&mut f, &profiles, &k, &apps);
        }
        ReadyElsewhere => {
            apps = Apps::default();
            play_on(&mut f, &profiles, &k, &apps);
            f.state = NowState::ReadyPlayingElsewhere { speaker: k.clone(), app: "Chrome".into() };
            if let Some(g) = f.speakers.iter_mut().find(|s| s.id.as_str() == "garage") {
                g.hidden = true;
            }
        }
        BusyQuestion => {
            if let Some(s) = f.speakers.iter_mut().find(|s| s.id == k) {
                s.busy = true;
                s.question = Some(TakeOver { other_pc: None });
            }
            f.speakers.retain(|s| s.id.as_str() != "garage");
        }
        NotTaking => {
            play_on(&mut f, &profiles, &k, &apps);
            f.state = NowState::NotTaking { speaker: k.clone(), since: t(21, 14) };
            f.seen = false;
            f.speakers.retain(|s| s.id.as_str() != "garage");
        }
        NotResponding => {
            play_on(&mut f, &profiles, &k, &apps);
            f.delay = None;
            f.volume.place = None;
            f.sending_sound = false;
            f.state = NowState::NotResponding {
                speaker: k.clone(),
                dropout: rules::dropout(1, false, AfterTries::BackToPc),
            };
        }
        StaySilent => {
            play_on(&mut f, &profiles, &k, &apps);
            f.delay = None;
            f.volume.place = None;
            f.sending_sound = false;
            f.state = NowState::NotResponding { speaker: k.clone(), dropout: rules::dropout(3, true, AfterTries::StaySilent) };
            f.speakers.retain(|s| s.id.as_str() != "garage");
            f.notification = Some(NotificationFacts {
                seq: 1,
                kind: NotificationKind::StoppedRespondingSilent,
                speaker: Some(k.clone()),
            });
        }
        FellBack => {
            f.state = NowState::FellBack {
                speaker: k.clone(),
                at: t(21, 14),
                cause: FellBackCause::StoppedResponding,
                returns: Return::WillAsk,
            };
            f.seen = false;
            f.speakers.retain(|s| s.id.as_str() != "garage");
            f.notification =
                Some(NotificationFacts { seq: 1, kind: NotificationKind::StoppedResponding, speaker: Some(k.clone()) });
        }
        SpeakerBack => {
            f.state = NowState::SpeakerBack { speaker: k.clone(), because: StayedBecause::PlayedAfterDropout };
            f.seen = false;
            f.speakers.retain(|s| s.id.as_str() != "garage");
            f.notification =
                Some(NotificationFacts { seq: 2, kind: NotificationKind::SpeakerBack, speaker: Some(k.clone()) });
        }
        Taken => {
            f.state = NowState::Taken { speaker: k.clone(), at: t(21, 40) };
            f.seen = false;
            f.speakers.retain(|s| s.id.as_str() != "garage");
            if let Some(s) = f.speakers.iter_mut().find(|s| s.id == k) {
                s.busy = true;
            }
            f.notification = Some(NotificationFacts { seq: 1, kind: NotificationKind::Taken, speaker: Some(k.clone()) });
        }
        NeedsPairing => {
            show(&mut f, "den");
            f.state = NowState::NeedsSecret { speaker: id("den"), secret: Secret::Code };
            f.remembered = Remembered::Speaker { name: "Den TV".into() };
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
            f.notification = Some(NotificationFacts {
                seq: 1,
                kind: NotificationKind::NeedsSecret(Secret::Code),
                speaker: Some(id("den")),
            });
        }
        LookingForSpeaker => {
            if let Some(s) = f.speakers.iter_mut().find(|s| s.id == k) {
                s.presence = Presence::NotFound;
            }
            f.state = NowState::LookingFor { speaker: k.clone(), looking: Looking::Searching };
        }
        CantFind => {
            if let Some(s) = f.speakers.iter_mut().find(|s| s.id == k) {
                s.presence = Presence::NotFound;
            }
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
            f.state = NowState::LookingFor { speaker: k.clone(), looking: Looking::NotFound(Return::ByItself) };
        }
        WindowsSwitched => {
            f.pc.output = output("headphones", "Headphones", "WH-1000XM4", OutputKind::Headphones);
            f.pc.outputs.push(f.pc.output.clone());
            f.volume.pc = Some(Level::at(24));
            f.state = NowState::WindowsSwitched { speaker: k.clone(), at: t(21, 50) };
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
        }
        ChooseSpeaker => {
            f.state = NowState::ChooseSpeaker;
            f.remembered = Remembered::Nothing;
            set_change(&mut f, "kitchen", 0);
            f.speakers.retain(|s| s.id.as_str() != "garage");
            f.music_assistant = None;
        }
        WindowsRefused => {
            play_on(&mut f, &profiles, &k, &apps);
            f.state = NowState::WindowsRefused { speaker: k.clone() };
            f.sending_sound = false;
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
        }
        MaPlaying => {
            apps = Apps::of(&["Edge"]);
            f.state = NowState::MaPlaying { apps: apps.clone(), room: Some("Living Room".into()) };
            f.volume.place = Some(Level::at(70));
            f.sending_sound = true;
            f.remembered = Remembered::MusicAssistant;
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
        }
        MaWaiting => {
            f.state = NowState::MaWaiting;
            f.remembered = Remembered::MusicAssistant;
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
        }
        MaAway => {
            f.state = NowState::MaAway { returns: Return::ByItself };
            f.music_assistant = Some(MaFacts { connected: false, last_seen: Some(t(21, 12)) });
            f.remembered = Remembered::MusicAssistant;
            f.seen = false;
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
        }
        MaBack => {
            f.state = NowState::MaBack;
            f.remembered = Remembered::MusicAssistant;
            f.seen = false;
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
        }
        OutputMissing => {
            f.state = NowState::OutputMissing;
            f.seen = false;
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
            f.music_assistant = None;
            f.notification = Some(NotificationFacts { seq: 1, kind: NotificationKind::OutputMissing, speaker: None });
        }
        SentApps => {
            f.mode = Mode::OnlySentApps { pass_through: false };
            play_on(&mut f, &profiles, &k, &apps);
            f.state = NowState::PlayingSentApps { speaker: k.clone(), apps: apps.clone() };
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
        }
        NoAppSent => {
            f.mode = Mode::OnlySentApps { pass_through: false };
            apps = Apps::default();
            play_on(&mut f, &profiles, &k, &apps);
            f.state = NowState::ReadyNoAppSent { speaker: k.clone() };
            f.speakers.retain(|s| s.id.as_str() != "garage" && s.id.as_str() != "office");
        }
        NothingFound => {
            f.speakers.clear();
            f.music_assistant = None;
            f.remembered = Remembered::Nothing;
            f.search = SearchFacts {
                phase: SearchPhase::NoneFound,
                network: Some(NetworkLabel { adapter: "Wi-Fi".into(), network: Some("HomeNet".into()) }),
                causes: vec![SearchCause::PublicNetwork { network: "HomeNet".into() }],
            };
        }
        Twins => {
            // Kitchen: a Sonos speaker, also reached by AirPlay; played last
            // by AirPlay. Office: an AirPlay speaker also reached by UPnP,
            // never played from here.
            let mut kitchen_airplay = speaker("kitchen-airplay", &f.speakers[0].name, SpeakerKind::AirPlay, "192.168.1.40", 2000);
            kitchen_airplay.twin = Some(k.clone());
            kitchen_airplay.last_used = Some(2);
            f.speakers[0].twin = Some(kitchen_airplay.id.clone());
            f.speakers[0].last_used = Some(1);
            let mut office_upnp =
                speaker("office-upnp", &f.speakers[3].name, SpeakerKind::Other { maker: Some("Denon".into()) }, "192.168.1.43", 300);
            office_upnp.twin = Some(office.clone());
            f.speakers[3].twin = Some(office_upnp.id.clone());
            profiles.insert(kitchen_airplay.id.clone(), airplay_profile(30, false, false));
            profiles.insert(
                office_upnp.id.clone(),
                Profile {
                    applies: Applies::Live,
                    range: SONOS,
                    choice: None,
                    fixed_on_steady: false,
                    level: Level::at(30),
                    needs_code: false,
                },
            );
            let airplay_kitchen = kitchen_airplay.id.clone();
            f.speakers.push(kitchen_airplay);
            f.speakers.push(office_upnp);
            play_on(&mut f, &profiles, &airplay_kitchen, &apps);
        }
        PcOutputsClosed | PcOutputsOpen => {
            let speakers = output("realtek", "Speakers", "Realtek Audio", OutputKind::Speakers);
            f.pc.outputs = vec![
                speakers.clone(),
                output("headphones", "Headphones", "Realtek Audio", OutputKind::Headphones),
                output("lg-tv", "LG TV", "NVIDIA High Definition Audio", OutputKind::Display),
            ];
            f.pc.output = speakers;
            f.pc.outputs_open = scenario == PcOutputsOpen;
        }
        CrowdedNetwork => {
            let desk = speaker("desk-office", "Desk", SpeakerKind::Sonos, "10.0.4.20", 200);
            profiles.insert(
                desk.id.clone(),
                Profile {
                    applies: Applies::Live,
                    range: SONOS,
                    choice: None,
                    fixed_on_steady: false,
                    level: Level { value: 30, muted: false, limit: Some(70) },
                    needs_code: false,
                },
            );
            let mut speakers = vec![desk];
            let mut absent = speaker("kitchen", "Kitchen", SpeakerKind::Sonos, "10.0.4.21", 200);
            absent.presence = Presence::NotFound;
            speakers.push(absent);
            for i in 1..=23 {
                let mut s = speaker(
                    &format!("room{i}"),
                    &format!("Meeting Room {i}"),
                    SpeakerKind::Sonos,
                    &format!("10.0.4.{}", 30 + i),
                    200,
                );
                s.used_here = false;
                speakers.push(s);
            }
            f.speakers = speakers;
            f.music_assistant = None;
            apps = Apps::of(&["Edge"]);
            play_on(&mut f, &profiles, &id("desk-office"), &apps);
            f.delay_hint_retired = false;
        }
        RowCatalogue => {
            play_on(&mut f, &profiles, &id("living"), &Apps::of(&["Edge"]));
            f.delay_hint_retired = false;
            let mut rows = vec![];
            let mut den = speaker("den", "Den TV", SpeakerKind::AppleTv, "192.168.1.45", 2000);
            den.failure = Some(ConnectFailure::NoAnswer);
            rows.push(den);
            let mut bedroom = speaker("bedroom", "Bedroom", SpeakerKind::HomePodPair, "192.168.1.42", 200);
            bedroom.presence = Presence::WaitingForPair;
            rows.push(bedroom);
            rows.push(speaker("kitchen-a", "Kitchen", SpeakerKind::Sonos, "192.168.1.20", 200));
            rows.push(speaker("kitchen-b", "Kitchen", SpeakerKind::Sonos, "192.168.1.31", 200));
            let mut bath = speaker("bathroom", "Bathroom", SpeakerKind::AirPlay, "192.168.1.50", 2000);
            bath.presence = Presence::CantPlay;
            rows.push(bath);
            let mut office = speaker("office", "Office", SpeakerKind::AirPlay, "192.168.1.43", 1740);
            office.delay = Some(SpeakerDelay { own_ms: 1740, measured: true, change_ms: 0 });
            rows.push(office);
            let mut garage = speaker("garage", "Garage", SpeakerKind::AirPlay, "192.168.1.44", 2000);
            garage.presence = Presence::NotFound;
            rows.push(garage);
            for i in 1..=12 {
                let mut s = speaker(
                    &format!("near{i}"),
                    &format!("Neighbour Speaker {i}"),
                    SpeakerKind::Sonos,
                    &format!("192.168.1.{}", 100 + i),
                    200,
                );
                s.used_here = false;
                rows.push(s);
            }
            f.speakers.retain(|s| s.id.as_str() == "living");
            f.speakers.extend(rows);
            f.music_assistant = None;
        }
        ApplyingDelay => {
            apps = Apps::of(&["Edge"]);
            play_on(&mut f, &profiles, &office, &apps);
            use_short(&mut f, &mut profiles, &office);
            set_change(&mut f, "office", -50);
            f.state = NowState::ApplyingDelay { speaker: office.clone(), from_ms: 0, to_ms: -50 };
            f.sending_sound = false;
            note(&mut f, Progress::Reconnecting, None);
        }
        DelayOwn => {
            set_change(&mut f, "kitchen", 0);
            f.delay_hint_retired = false;
            play_on(&mut f, &profiles, &k, &apps);
        }
        DelayRemoving => {
            set_change(&mut f, "kitchen", -125);
            play_on(&mut f, &profiles, &k, &apps);
            note(&mut f, Progress::Live { to_go_ms: -75 }, None);
        }
        DelayAtLimit => {
            set_change(&mut f, "kitchen", -200);
            play_on(&mut f, &profiles, &k, &apps);
        }
        DelayAdding => {
            set_change(&mut f, "kitchen", 100);
            play_on(&mut f, &profiles, &k, &apps);
            note(&mut f, Progress::Live { to_go_ms: 50 }, None);
        }
        DelayGoingBack => {
            set_change(&mut f, "kitchen", 0);
            play_on(&mut f, &profiles, &k, &apps);
            note(&mut f, Progress::Live { to_go_ms: 125 }, None);
        }
        DelayReconnectPending => {
            apps = Apps::of(&["Edge"]);
            play_on(&mut f, &profiles, &office, &apps);
            use_short(&mut f, &mut profiles, &office);
            set_change(&mut f, "office", -50);
            note(&mut f, Progress::ReconnectPending, None);
        }
        DelayNotAccepted => {
            apps = Apps::of(&["Edge"]);
            set_change(&mut f, "office", -200);
            play_on(&mut f, &profiles, &office, &apps);
            note(&mut f, Progress::Settled, Some(DelayNote::NotAccepted { tried_ms: -300, back_to_ms: -200 }));
        }
        DelaySteadyRemove => {
            apps = Apps::of(&["Edge"]);
            set_change(&mut f, "office", -100);
            play_on(&mut f, &profiles, &office, &apps);
            note(&mut f, Progress::Settled, Some(DelayNote::SteadyRemovePressed));
        }
        DelaySwitchedToShort => {
            apps = Apps::of(&["Edge"]);
            play_on(&mut f, &profiles, &office, &apps);
            use_short(&mut f, &mut profiles, &office);
            note(&mut f, Progress::Settled, Some(DelayNote::ChoiceSwitched { other_kept_ms: -300 }));
        }
        DelayFixedOnSteady => {
            apps = Apps::of(&["Edge"]);
            show(&mut f, "den");
            play_on(&mut f, &profiles, &id("den"), &apps);
        }
        DelaySendspin => {
            apps = Apps::of(&["Edge"]);
            show(&mut f, "desk");
            play_on(&mut f, &profiles, &id("desk"), &apps);
        }
        DelayDroppedOut => {
            set_change(&mut f, "kitchen", -150);
            play_on(&mut f, &profiles, &k, &apps);
            note(&mut f, Progress::Settled, Some(DelayNote::DroppedOut { with_ms: -200, back_to_ms: -150 }));
        }
        DelayDropouts => {
            set_change(&mut f, "kitchen", 0);
            play_on(&mut f, &profiles, &k, &apps);
            note(&mut f, Progress::Settled, Some(DelayNote::Dropouts { count: 3 }));
        }
        NoticeRestart => f.notices.restart_pending = true,
        NoticeAdapterOff => f.notices.adapter_off = Some("Wi-Fi".into()),
        NoticeRefused => {
            play_on(&mut f, &profiles, &k, &apps);
            f.notices.refused_in_group =
                Some(RefusedDevice { name: "Kitchen Sub".into(), key: "192.168.1.51".into() });
        }
        NoticeLab => f.notices.lab_on = true,
        NoticeUpdate => f.notices.update_ready = Some("0.2.1".into()),
        NoticeUpdated => f.notices.updated_to = Some("0.2.1".into()),
        NoticeDonation => f.notices.donation_due = true,
        FirstRunPick => {
            f.speakers.retain(|s| s.id.as_str() != "garage");
            for s in &mut f.speakers {
                s.used_here = false;
                if let Some(d) = s.delay.as_mut() {
                    d.change_ms = 0;
                }
            }
            f.music_assistant = None;
            f.remembered = Remembered::Nothing;
            f.first_run = Some(FirstRun::Pick);
        }
        FirstRunHear | FirstRunFindIt => {
            set_change(&mut f, "kitchen", 0);
            f.speakers.retain(|s| s.id.as_str() != "garage");
            f.music_assistant = None;
            play_on(&mut f, &profiles, &k, &Apps::default());
            if let Some(l) = f.volume.place.as_mut() {
                l.value = 25;
            }
            f.first_run = Some(if scenario == FirstRunHear {
                FirstRun::Hear { speaker: k.clone() }
            } else {
                FirstRun::FindIt { speaker: k.clone(), start_at_sign_in: true }
            });
            apps = Apps::default();
        }
    }
    // Den TV and the desk speaker are only in the scenarios that show them.
    f.speakers.retain(|s| !(s.hidden && matches!(s.id.as_str(), "den" | "desk")));
    (f, profiles, apps)
}

/// Switches an AirPlay speaker of a scenario to Short delay.
fn use_short(f: &mut Facts, profiles: &mut HashMap<SpeakerId, Profile>, sid: &SpeakerId) {
    let Some(p) = profiles.get_mut(sid) else { return };
    let Some((choice, _, short)) = p.choice.as_mut() else { return };
    choice.current = DelayChoice::Short;
    p.range = *short;
    p.applies = Applies::Reconnect;
    if let Some(s) = f.speakers.iter_mut().find(|s| &s.id == sid) {
        s.delay = Some(SpeakerDelay { own_ms: choice.short_ms, measured: false, change_ms: 0 });
    }
    if f.delay.as_ref().is_some_and(|d| &d.speaker == sid) {
        f.delay = Some(control_facts(sid, p));
    }
}
