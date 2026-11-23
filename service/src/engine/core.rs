//! The engine's decisions, without threads, I/O or a clock: what the person
//! chose, what plays, the connect in flight, and the one situation the
//! sound is in (being retried, back on this PC, looking for the speaker…).
//! Commands and events go in with the time and what the driver observes
//! ([`World`]); what to do comes out ([`Do`]). The state every surface
//! shows is derived from this and the world by [`Core::state`].

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::model::command::{Command, Effect, Refusal};
use crate::model::facts::{ComesBack, ConnectFailure, NotificationFacts, NotificationKind, TakeOver};
use crate::model::rules::{self, AutoMove, MoveFacts, NotifyContext, RetryStep, Trigger};
use crate::model::state::{
    AfterTries, Apps, ConnectStage, Dropout, FellBackCause, NowState, PcOutputId, Place, Return, Secret,
    SoundAt, SpeakerId, StayedBecause,
};
use crate::model::text::LocalTime;

/// Where a connect comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A person's pick in the app's own window. Only such a pick may play
    /// to a receiver that announces another speaker's identity, and only
    /// there can a code or password be asked for.
    Window,
    /// A person's choice from anywhere else: the tray, a shortcut, a
    /// notification, the phone page, the command line, the web API.
    Elsewhere,
    /// The app's own decision: a return, a retry, start-up, Windows
    /// choosing the app's output.
    ByItself,
}

/// A connect the driver runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub gen: u64,
    pub place: Place,
    pub origin: Origin,
    /// Ask the speaker first whether it is playing something else.
    pub check_busy: bool,
}

/// What the driver does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Do {
    Connect(Job),
    /// The connect in flight is no longer wanted.
    Supersede,
    /// Let the bound speaker go; `tell_speaker` false when someone else
    /// is using it now.
    Release { tell_speaker: bool },
    /// Make this app's output Windows' default, then report how it went.
    UseOurOutput,
    /// Windows' output back to this PC's (to `Core::pc_output`).
    GiveBack,
    /// Remember the person's choice across restarts.
    SaveChoice(Option<Place>),
    /// "Has anything played on this PC since" counts from now.
    ResetPlayed,
    /// A new notification is in `Core::notification`.
    Notify,
}

/// How a connect ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    Bound,
    /// The speaker plays something else; nothing was sent to it.
    Busy { other_pc: Option<String> },
    Failed(ConnectFailure),
    /// It wants a code or password and nobody was asked.
    NeedsSecret(Secret),
    /// It wants a code and the dialog is open.
    Pairing,
    Cancelled,
}

/// Something that happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Stage { gen: u64, stage: ConnectStage },
    Ended { gen: u64, ended: Ended },
    /// The bound speaker stopped responding.
    Dropped,
    /// The bound speaker ended the session for good: it refused this PC's
    /// pairing.
    PairingRefused,
    /// The bound speaker plays something else now.
    Taken,
    /// After a switch to this app's output: whether Windows took it.
    SwitchRead { ok: bool },
    /// Windows' default output changed (the world has the new one).
    DefaultChanged,
    Suspending,
    Resumed,
    SessionEnding,
    /// The app started; `after_update` when it closed for an update.
    Started { after_update: bool },
}

/// The person's settings the rules need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// General → *Keep trying, and stay silent*.
    pub stay_silent: bool,
    /// General → *Tell me when sound leaves the place I chose*.
    pub tell_me: bool,
    /// General → *…and go back to the speaker I was using*.
    pub go_back_at_start: bool,
    /// Each speaker's *Comes back by itself* (absent: the default).
    pub comes_back: HashMap<SpeakerId, ComesBack>,
    /// General → *Only apps I send there*: Windows' output is never moved.
    pub sent_apps_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            stay_silent: false,
            tell_me: true,
            go_back_at_start: true,
            comes_back: HashMap::new(),
            sent_apps_mode: false,
        }
    }
}

impl Settings {
    fn comes_back(&self, id: &SpeakerId) -> ComesBack {
        self.comes_back.get(id).copied().unwrap_or(ComesBack::LastChosen)
    }

    fn after_tries(&self) -> AfterTries {
        if self.stay_silent {
            AfterTries::StaySilent
        } else {
            AfterTries::BackToPc
        }
    }
}

/// What the driver observes at a moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct World {
    /// Speakers listed and playable now.
    pub found: HashSet<SpeakerId>,
    /// This PC sends real sound to the bound place.
    pub sending: bool,
    /// The apps with sound on this app's output.
    pub apps_here: Apps,
    /// An app with sound on the PC's own output while a speaker is bound.
    pub app_elsewhere: Option<String>,
    /// Something is audible on this PC right now.
    pub playing_on_pc: bool,
    /// Something has played on this PC since the moment that counts (see
    /// [`Do::ResetPlayed`]).
    pub played_since: bool,
    pub default_output: Option<PcOutputId>,
    pub our_output: Option<PcOutputId>,
    /// The platform can tell Windows' outputs (else nothing is switched).
    pub knows_outputs: bool,
    /// An AirPlay speaker reconnecting to apply a delay change (from, to).
    pub delay_reconnect: Option<(i32, i32)>,
    /// The bound speaker stopped asking for the sound at this time.
    pub not_taking_since: Option<LocalTime>,
    pub ma_connected: bool,
    pub ma_playing: bool,
    pub ma_room: Option<String>,
    /// A code dialog is open.
    pub pin_ceremony: bool,
    /// The time on the PC's clock.
    pub time: LocalTime,
    pub settings: Settings,
}

#[derive(Clone, Debug)]
struct Connecting {
    gen: u64,
    place: Place,
    from: SoundAt,
    stage: ConnectStage,
}

#[derive(Clone, Debug, PartialEq)]
enum Special {
    None,
    Retrying { id: SpeakerId, dropped_at: Instant, failed: u8, quick: bool, next_at: Instant, then: AfterTries },
    FellBack { id: SpeakerId, at: LocalTime, cause: FellBackCause, gone: bool },
    Back { id: SpeakerId, because: StayedBecause },
    Taken { id: SpeakerId, at: LocalTime },
    Secret { id: SpeakerId, secret: Secret },
    Looking { id: SpeakerId, since: Instant, trigger: Trigger },
    Switched { place: Place, at: LocalTime, release_at: Option<Instant> },
    Choose,
    Refused,
    MaAway,
    MaBack,
}

/// The engine's decisions.
pub struct Core {
    /// What plays: a speaker or Music Assistant; `None` on this PC.
    bound: Option<Place>,
    special: Special,
    connect: Option<Connecting>,
    /// An automatic connect in flight (a retry or a return).
    auto: Option<(u64, Place)>,
    gen: u64,
    /// The person's last choice, kept across restarts.
    pub choice: Option<Place>,
    /// The last speaker (or Music Assistant) chosen: Windows' device is
    /// named after it.
    pub named: Option<Place>,
    /// The PC's output to give the sound back to.
    pub pc_output: Option<PcOutputId>,
    /// The default output a switch of ours is expected to produce.
    expect_default: Option<PcOutputId>,
    pub failures: HashMap<SpeakerId, ConnectFailure>,
    pub questions: HashMap<SpeakerId, TakeOver>,
    /// The panel has been opened since the current state began.
    pub seen: bool,
    panel_open: bool,
    settings_open: bool,
    pub notification: Option<NotificationFacts>,
    seq: u64,
    last_notified: HashMap<SpeakerId, (NotificationKind, Instant)>,
    fell_back_at: Option<Instant>,
    /// Something was playing when the current trouble began.
    was_playing: bool,
    /// "Whenever it appears" speakers already tried in this appearance.
    tried_appear: HashSet<SpeakerId>,
    epoch: Instant,
}

const KEEP_TRYING_EVERY: Duration = Duration::from_millis(rules::KEEP_TRYING_EVERY_MS);
const RELEASE_AFTER: Duration = Duration::from_millis(rules::RELEASE_AFTER_WINDOWS_SWITCH_MS);

impl Core {
    pub fn new(choice: Option<Place>, named: Option<Place>, now: Instant) -> Self {
        Core {
            bound: None,
            special: Special::None,
            connect: None,
            auto: None,
            gen: 0,
            choice,
            named,
            pc_output: None,
            expect_default: None,
            failures: HashMap::new(),
            questions: HashMap::new(),
            seen: true,
            panel_open: false,
            settings_open: false,
            notification: None,
            seq: 0,
            last_notified: HashMap::new(),
            fell_back_at: None,
            was_playing: false,
            tried_appear: HashSet::new(),
            epoch: now,
        }
    }

    /// What plays now (a speaker or Music Assistant).
    pub fn bound(&self) -> Option<&Place> {
        self.bound.as_ref()
    }

    /// Take what plays as it is when another path bound or ended it (a
    /// delay change's reconnect, a Sendspin pairing answer). Nothing changes
    /// while a connect of the engine's own is in flight.
    pub fn adopt(&mut self, place: Option<Place>) {
        if self.connect.is_some() || self.auto.is_some() || self.bound == place {
            return;
        }
        match (&place, &self.special) {
            // A dead session stays bound while it is retried.
            (None, Special::Retrying { .. }) => {}
            (Some(_), _) => {
                self.bound = place;
                if matches!(self.special, Special::Retrying { .. } | Special::Refused) {
                    self.special = Special::None;
                }
            }
            (None, _) => self.bound = None,
        }
    }

    /// The generation of the connect a person is waiting on, if any.
    pub fn connecting(&self) -> Option<(u64, &Place)> {
        self.connect.as_ref().map(|c| (c.gen, &c.place))
    }

    /// The state every surface shows.
    pub fn state(&self, w: &World, now: Instant) -> NowState {
        if w.knows_outputs && w.our_output.is_none() {
            return NowState::OutputMissing;
        }
        if let Some(c) = &self.connect {
            match &c.place {
                Place::Speaker(id) => {
                    return NowState::Connecting { target: id.clone(), from: c.from.clone(), stage: c.stage }
                }
                Place::MusicAssistant => return NowState::MaWaiting,
                Place::ThisPc => {}
            }
        }
        match &self.special {
            Special::Retrying { id, failed, quick, then, .. } => NowState::NotResponding {
                speaker: id.clone(),
                dropout: if *quick {
                    rules::dropout(*failed, false, *then)
                } else {
                    Dropout::KeepTrying { every_secs: (rules::KEEP_TRYING_EVERY_MS / 1000) as u16 }
                },
            },
            Special::FellBack { id, at, cause, .. } => NowState::FellBack {
                speaker: id.clone(),
                at: *at,
                cause: *cause,
                returns: rules::returns(w.settings.comes_back(id), w.played_since),
            },
            Special::Back { id, because } => NowState::SpeakerBack { speaker: id.clone(), because: *because },
            Special::Taken { id, at } => NowState::Taken { speaker: id.clone(), at: *at },
            Special::Secret { id, secret } => NowState::NeedsSecret { speaker: id.clone(), secret: *secret },
            Special::Looking { id, since, .. } => NowState::LookingFor {
                speaker: id.clone(),
                looking: rules::looking(
                    now.saturating_duration_since(*since).as_millis() as u64,
                    w.settings.comes_back(id),
                    w.played_since,
                ),
            },
            Special::Switched { place: Place::Speaker(id), at, .. } => {
                NowState::WindowsSwitched { speaker: id.clone(), at: *at }
            }
            Special::Switched { .. } => NowState::OnThisPc,
            Special::Choose => NowState::ChooseSpeaker,
            Special::Refused => match &self.bound {
                Some(Place::Speaker(id)) => NowState::WindowsRefused { speaker: id.clone() },
                _ => self.settled(w),
            },
            Special::MaAway => NowState::MaAway { returns: if w.played_since { Return::WillAsk } else { Return::ByItself } },
            Special::MaBack => NowState::MaBack,
            Special::None => self.settled(w),
        }
    }

    /// The state when nothing special is going on: what plays, as observed.
    fn settled(&self, w: &World) -> NowState {
        match &self.bound {
            None | Some(Place::ThisPc) => NowState::OnThisPc,
            Some(Place::MusicAssistant) => {
                if w.ma_playing {
                    NowState::MaPlaying { apps: w.apps_here.clone(), room: w.ma_room.clone() }
                } else {
                    NowState::MaWaiting
                }
            }
            Some(Place::Speaker(id)) => {
                let speaker = id.clone();
                if let Some((from_ms, to_ms)) = w.delay_reconnect {
                    if w.sending || !w.apps_here.0.is_empty() {
                        return NowState::ApplyingDelay { speaker, from_ms, to_ms };
                    }
                }
                if let Some(since) = w.not_taking_since {
                    return NowState::NotTaking { speaker, since };
                }
                if w.settings.sent_apps_mode {
                    return if w.apps_here.0.is_empty() {
                        NowState::ReadyNoAppSent { speaker }
                    } else {
                        NowState::PlayingSentApps { speaker, apps: w.apps_here.clone() }
                    };
                }
                if w.sending {
                    NowState::Playing { speaker, apps: w.apps_here.clone() }
                } else if let Some(app) = &w.app_elsewhere {
                    NowState::ReadyPlayingElsewhere { speaker, app: app.clone() }
                } else {
                    NowState::Ready { speaker }
                }
            }
        }
    }

    fn sound_at(&self) -> SoundAt {
        match &self.bound {
            Some(Place::Speaker(id)) => SoundAt::Speaker(id.clone()),
            Some(Place::MusicAssistant) => SoundAt::MusicAssistant,
            _ => SoundAt::ThisPc,
        }
    }

    fn next_gen(&mut self) -> u64 {
        self.gen += 1;
        self.gen
    }

    /// A command from a surface: `origin` is [`Origin::Window`] for the
    /// app's own window, else [`Origin::Elsewhere`].
    pub fn command(
        &mut self,
        cmd: &Command,
        origin: Origin,
        w: &World,
        now: Instant,
    ) -> (Result<Effect, Refusal>, Vec<Do>) {
        let before = self.state(w, now);
        let mut out = Vec::new();
        match cmd {
            Command::Play(Place::Speaker(id)) => self.choose(Place::Speaker(id.clone()), false, origin, &mut out),
            Command::PlayTakingOver(id) => self.choose(Place::Speaker(id.clone()), true, origin, &mut out),
            Command::Play(Place::MusicAssistant) => self.choose(Place::MusicAssistant, false, origin, &mut out),
            Command::Play(Place::ThisPc) | Command::StayOnThisPc => {
                self.to_this_pc(&mut out);
                self.choice = Some(Place::ThisPc);
                out.push(Do::SaveChoice(self.choice.clone()));
            }
            Command::Release { .. } | Command::Quit => self.to_this_pc(&mut out),
            Command::CancelConnect => {
                if self.connect.take().is_some() {
                    out.push(Do::Supersede);
                }
            }
            Command::DismissQuestion(id) => {
                self.questions.remove(id);
            }
            Command::PanelShown(shown) => {
                self.panel_open = *shown;
                if *shown {
                    self.seen = true;
                } else if self.special == Special::Choose {
                    // Closed without a choice: Windows plays on this PC again.
                    self.special = Special::None;
                    self.give_back(&mut out);
                }
            }
            Command::SettingsShown(shown) => self.settings_open = *shown,
            _ => {}
        }
        self.settle(before, w, now, &mut out);
        (Ok(Effect::Done), out)
    }

    /// `place` (a speaker or Music Assistant) is chosen: by a person, or
    /// by the app as if by one (Windows choosing the app's output).
    fn choose(&mut self, place: Place, take_over: bool, origin: Origin, out: &mut Vec<Do>) {
        if let Place::Speaker(id) = &place {
            self.failures.remove(id);
            if take_over {
                self.questions.remove(id);
            }
        }
        if !take_over {
            self.questions.clear();
        }
        let in_flight = self.connect.as_ref().is_some_and(|c| c.place == place);
        let here = self.bound.as_ref() == Some(&place)
            && matches!(self.special, Special::None | Special::Refused | Special::Switched { .. });
        if here && !take_over && self.connect.is_some() {
            // The newest choice is where the sound already is.
            self.connect = None;
            out.push(Do::Supersede);
            return;
        }
        if in_flight || (here && !take_over) {
            if matches!(self.special, Special::Switched { .. }) {
                // Picked again in the app after Windows moved: back to it.
                self.special = Special::None;
                self.use_our_output_later(out);
            }
            return;
        }
        if self.connect.is_some() || self.auto.is_some() {
            out.push(Do::Supersede);
            self.auto = None;
        }
        self.special = Special::None;
        let gen = self.next_gen();
        self.connect = Some(Connecting { gen, place: place.clone(), from: self.sound_at(), stage: ConnectStage::Finding });
        out.push(Do::Connect(Job { gen, place, origin, check_busy: !take_over }));
    }

    fn auto_connect(&mut self, place: Place, out: &mut Vec<Do>) {
        let gen = self.next_gen();
        self.auto = Some((gen, place.clone()));
        out.push(Do::Connect(Job { gen, place, origin: Origin::ByItself, check_busy: true }));
    }

    /// Sound back on this PC, nothing bound, nothing pending.
    fn to_this_pc(&mut self, out: &mut Vec<Do>) {
        if self.connect.take().is_some() || self.auto.take().is_some() {
            out.push(Do::Supersede);
        }
        // Windows' output first: the sound is back here before the
        // speaker stops.
        self.give_back(out);
        if self.bound.take().is_some() {
            out.push(Do::Release { tell_speaker: true });
        }
        self.special = Special::None;
        self.questions.clear();
    }

    fn give_back(&mut self, out: &mut Vec<Do>) {
        self.expect_default = self.pc_output.clone();
        out.push(Do::GiveBack);
    }

    fn use_our_output_later(&mut self, out: &mut Vec<Do>) {
        out.push(Do::UseOurOutput);
    }

    /// Something happened.
    pub fn event(&mut self, ev: Event, w: &World, now: Instant) -> Vec<Do> {
        let before = self.state(w, now);
        let mut out = Vec::new();
        match ev {
            Event::Stage { gen, stage } => {
                if let Some(c) = self.connect.as_mut().filter(|c| c.gen == gen) {
                    c.stage = stage;
                }
            }
            Event::Ended { gen, ended } => self.ended(gen, ended, w, now, &mut out),
            Event::Dropped => {
                if let (Some(Place::Speaker(id)), true) =
                    (&self.bound, matches!(self.special, Special::None | Special::Refused))
                {
                    self.was_playing = w.sending || w.playing_on_pc;
                    self.special = Special::Retrying {
                        id: id.clone(),
                        dropped_at: now,
                        failed: 0,
                        quick: true,
                        next_at: now + Duration::from_millis(rules::QUICK_TRIES_AT_MS[0]),
                        then: w.settings.after_tries(),
                    };
                }
            }
            Event::PairingRefused => {
                if let Some(Place::Speaker(id)) = self.bound.clone() {
                    self.was_playing = w.sending || w.playing_on_pc;
                    self.fall_back(id, FellBackCause::RefusedPairing, w, now, &mut out);
                }
            }
            Event::Taken => {
                if let Some(Place::Speaker(id)) = self.bound.clone() {
                    self.was_playing = w.sending || w.playing_on_pc;
                    self.step_aside(id, w, &mut out);
                    out.push(Do::Release { tell_speaker: false });
                }
            }
            Event::SwitchRead { ok } => {
                // The switch is over: a later change is the person's or
                // Windows'.
                self.expect_default = None;
                if !ok && self.bound.is_some() && self.special == Special::None {
                    self.special = Special::Refused;
                } else if ok && self.special == Special::Refused {
                    self.special = Special::None;
                }
            }
            Event::DefaultChanged => self.default_changed(w, now, &mut out),
            Event::Suspending | Event::SessionEnding => {
                // Sleep and sign-out give the sound back first; the choice
                // stays, for the return at wake or sign-in.
                self.to_this_pc(&mut out);
            }
            Event::Resumed => self.start_looking(Trigger::SessionStart, w, now, &mut out),
            Event::Started { after_update } => {
                let trigger = if after_update { Trigger::BackAfterUpdate } else { Trigger::SessionStart };
                self.start_looking(trigger, w, now, &mut out);
            }
        }
        self.settle(before, w, now, &mut out);
        out
    }

    fn start_looking(&mut self, trigger: Trigger, w: &World, now: Instant, out: &mut Vec<Do>) {
        out.push(Do::ResetPlayed);
        match &self.choice {
            Some(Place::Speaker(id)) => {
                let allowed = trigger == Trigger::BackAfterUpdate
                    || (w.settings.go_back_at_start && w.settings.comes_back(id) != ComesBack::Never);
                if allowed {
                    self.special = Special::Looking { id: id.clone(), since: now, trigger };
                }
            }
            Some(Place::MusicAssistant) if w.settings.go_back_at_start || trigger == Trigger::BackAfterUpdate => {
                self.auto_connect(Place::MusicAssistant, out);
            }
            _ => {}
        }
    }

    fn ended(&mut self, gen: u64, ended: Ended, w: &World, now: Instant, out: &mut Vec<Do>) {
        if let Some(c) = self.connect.clone().filter(|c| c.gen == gen) {
            // A connect a person is waiting on.
            match ended {
                Ended::Bound => {
                    self.connect = None;
                    self.bound_to(c.place, w, out);
                }
                Ended::Busy { other_pc } => {
                    self.connect = None;
                    if let Place::Speaker(id) = c.place {
                        self.questions.insert(id, TakeOver { other_pc });
                    }
                }
                Ended::Failed(f) => {
                    self.connect = None;
                    if let Place::Speaker(id) = c.place {
                        self.failures.insert(id, f);
                    }
                }
                Ended::NeedsSecret(secret) => {
                    self.connect = None;
                    if let Place::Speaker(id) = c.place {
                        self.failures.insert(
                            id,
                            if secret == Secret::Password { ConnectFailure::WrongPassword } else { ConnectFailure::Other },
                        );
                    }
                }
                Ended::Pairing => {
                    if let Some(c) = self.connect.as_mut() {
                        c.stage = ConnectStage::Pairing;
                    }
                }
                Ended::Cancelled => self.connect = None,
            }
            return;
        }
        let Some((_, place)) = self.auto.clone().filter(|(g, _)| *g == gen) else { return };
        self.auto = None;
        match ended {
            Ended::Bound => {
                if let Special::Retrying { .. } = self.special {
                    // Back after a dropout: no new choice was made.
                    self.special = Special::None;
                    self.bound = Some(place.clone());
                    self.use_our_output_later(out);
                } else {
                    self.special = Special::None;
                    self.bound_to(place, w, out);
                }
            }
            Ended::Busy { .. } => {
                if let Place::Speaker(id) = place {
                    match self.special {
                        // Someone else took it while it was being retried,
                        // or it is busy when it would come back: step aside.
                        Special::Retrying { .. } | Special::Looking { .. } | Special::FellBack { .. } => {
                            self.bound = None;
                            self.step_aside(id, w, out);
                        }
                        _ => {}
                    }
                }
            }
            Ended::NeedsSecret(secret) => {
                if let Place::Speaker(id) = place {
                    self.bound = None;
                    self.special = Special::Secret { id, secret };
                    self.give_back(out);
                }
            }
            Ended::Failed(_) | Ended::Pairing | Ended::Cancelled => {
                if let Special::Retrying { failed, quick, next_at, .. } = &mut self.special {
                    *failed = failed.saturating_add(1);
                    if !*quick {
                        *next_at = now + KEEP_TRYING_EVERY;
                    }
                }
            }
        }
    }

    /// `place` is now what plays.
    fn bound_to(&mut self, place: Place, w: &World, out: &mut Vec<Do>) {
        self.bound = Some(place.clone());
        self.special = Special::None;
        self.fell_back_at = None;
        self.choice = Some(place.clone());
        self.named = Some(place);
        out.push(Do::SaveChoice(self.choice.clone()));
        if w.knows_outputs && !w.settings.sent_apps_mode && w.default_output != w.our_output {
            if w.default_output.is_some() {
                self.pc_output = w.default_output.clone();
            }
            self.expect_default = w.our_output.clone();
            out.push(Do::UseOurOutput);
        }
    }

    fn fall_back(&mut self, id: SpeakerId, cause: FellBackCause, w: &World, now: Instant, out: &mut Vec<Do>) {
        if self.auto.take().is_some() {
            out.push(Do::Supersede);
        }
        if self.bound.take().is_some() {
            out.push(Do::Release { tell_speaker: true });
        }
        self.special = Special::FellBack { id, at: w.time, cause, gone: false };
        self.fell_back_at = Some(now);
        self.give_back(out);
        out.push(Do::ResetPlayed);
    }

    fn step_aside(&mut self, id: SpeakerId, w: &World, out: &mut Vec<Do>) {
        self.bound = None;
        self.special = Special::Taken { id, at: w.time };
        self.give_back(out);
    }

    fn default_changed(&mut self, w: &World, now: Instant, out: &mut Vec<Do>) {
        let Some(d) = w.default_output.clone() else { return };
        if self.expect_default.as_ref() == Some(&d) {
            self.expect_default = None;
            return;
        }
        self.expect_default = None;
        let ours = Some(&d) == w.our_output.as_ref();
        match (ours, self.bound.clone()) {
            (true, Some(_)) => {
                if matches!(self.special, Special::Switched { .. } | Special::Refused) {
                    self.special = Special::None;
                }
            }
            (true, None) => {
                if let Special::Switched { place, release_at: None, .. } = self.special.clone() {
                    // Windows came back after the speaker was let go.
                    self.choose(place, false, Origin::ByItself, out);
                } else if self.connect.is_none() {
                    // Windows is a second door: picking this app's output
                    // plays on the speaker it is named after.
                    match self.named.clone() {
                        Some(place @ (Place::Speaker(_) | Place::MusicAssistant)) => {
                            self.choose(place, false, Origin::ByItself, out)
                        }
                        _ => self.special = Special::Choose,
                    }
                }
            }
            (false, Some(place)) => {
                self.pc_output = Some(d);
                if !matches!(place, Place::ThisPc) {
                    self.special =
                        Special::Switched { place, at: w.time, release_at: Some(now + RELEASE_AFTER) };
                }
            }
            (false, None) => {
                // This PC's output follows Windows.
                self.pc_output = Some(d);
                if self.special == Special::Choose {
                    self.special = Special::None;
                }
            }
        }
    }

    /// Time passes: retries, returns, releases.
    pub fn tick(&mut self, w: &World, now: Instant) -> Vec<Do> {
        let before = self.state(w, now);
        let mut out = Vec::new();
        self.tried_appear.retain(|id| w.found.contains(id));
        match self.special.clone() {
            Special::Retrying { id, dropped_at, failed, quick, next_at, then } => {
                let elapsed = now.saturating_duration_since(dropped_at).as_millis() as u64;
                if quick {
                    match rules::next_retry(elapsed, failed, false, then) {
                        RetryStep::TryAt { at_ms, .. } => {
                            if self.auto.is_none() && elapsed >= at_ms {
                                self.auto_connect(Place::Speaker(id), &mut out);
                            }
                        }
                        RetryStep::FallBack => self.fall_back(id, FellBackCause::StoppedResponding, w, now, &mut out),
                        RetryStep::KeepTrying => {
                            if let Special::Retrying { quick, next_at, .. } = &mut self.special {
                                *quick = false;
                                *next_at = now + KEEP_TRYING_EVERY;
                            }
                        }
                    }
                } else if self.auto.is_none() && now >= next_at {
                    self.auto_connect(Place::Speaker(id), &mut out);
                }
            }
            Special::FellBack { id, gone, .. } => {
                let found = w.found.contains(&id);
                if !found && !gone {
                    if let Special::FellBack { gone, .. } = &mut self.special {
                        *gone = true;
                    }
                } else if found && gone && self.auto.is_none() {
                    let facts = self.move_facts(&id, w);
                    match rules::auto_move(Trigger::BackAfterDropout, facts) {
                        AutoMove::Return => self.auto_connect(Place::Speaker(id), &mut out),
                        AutoMove::Offer(because) => self.special = Special::Back { id, because },
                        _ => {}
                    }
                }
            }
            Special::Looking { id, trigger, .. } => {
                if self.auto.is_none() && self.connect.is_none() {
                    let mut facts = self.move_facts(&id, w);
                    facts.output_unchanged = w.default_output.is_none() || w.default_output == self.pc_output;
                    match rules::auto_move(trigger, facts) {
                        AutoMove::Return => self.auto_connect(Place::Speaker(id), &mut out),
                        AutoMove::Offer(because) => self.special = Special::Back { id, because },
                        AutoMove::FollowWindows | AutoMove::Stay => self.special = Special::None,
                        AutoMove::StepAside => self.step_aside(id, w, &mut out),
                        AutoMove::AskForSecret(secret) => self.special = Special::Secret { id, secret },
                        AutoMove::KeepLooking => {}
                    }
                }
            }
            Special::Switched { place, at, release_at: Some(t) } if now >= t => {
                self.special = Special::Switched { place, at, release_at: None };
                self.bound = None;
                out.push(Do::Release { tell_speaker: true });
            }
            Special::None if self.bound == Some(Place::MusicAssistant) && !w.ma_connected => {
                self.special = Special::MaAway;
                self.bound = None;
                out.push(Do::Release { tell_speaker: false });
                self.give_back(&mut out);
                out.push(Do::ResetPlayed);
            }
            Special::MaAway if w.ma_connected && self.auto.is_none() => {
                let facts = MoveFacts { found: true, ..self.move_facts(&SpeakerId::new(""), w) };
                match rules::auto_move(Trigger::MusicAssistantBack, facts) {
                    AutoMove::Return => {
                        self.special = Special::None;
                        self.auto_connect(Place::MusicAssistant, &mut out);
                    }
                    _ => self.special = Special::MaBack,
                }
            }
            Special::None if self.bound.is_none() && self.connect.is_none() && self.auto.is_none() => {
                // Speakers set to come back whenever they appear.
                let mut appeared: Vec<SpeakerId> = w
                    .found
                    .iter()
                    .filter(|id| w.settings.comes_back(id) == ComesBack::WheneverAppears)
                    .filter(|id| !self.tried_appear.contains(*id))
                    .cloned()
                    .collect();
                appeared.sort();
                if let Some(id) = appeared.into_iter().next() {
                    self.tried_appear.insert(id.clone());
                    if rules::auto_move(Trigger::Appeared, self.move_facts(&id, w)) == AutoMove::Return {
                        self.auto_connect(Place::Speaker(id), &mut out);
                    }
                }
            }
            _ => {}
        }
        // A code dialog closed without the speaker connecting.
        if self.connect.as_ref().is_some_and(|c| c.stage == ConnectStage::Pairing) && !w.pin_ceremony {
            self.connect = None;
        }
        self.settle(before, w, now, &mut out);
        out
    }

    fn move_facts(&self, id: &SpeakerId, w: &World) -> MoveFacts {
        MoveFacts {
            found: w.found.contains(id),
            busy: false,
            needs: None,
            played_here_since: w.played_since,
            playing_here_now: w.playing_on_pc,
            go_back_at_start: w.settings.go_back_at_start,
            comes_back: w.settings.comes_back(id),
            output_unchanged: true,
        }
    }

    /// After every step: notify if the change calls for it, and re-arm the
    /// badge of a state that lasts until it is seen.
    fn settle(&mut self, before: NowState, w: &World, now: Instant, out: &mut Vec<Do>) {
        let after = self.state(w, now);
        if after == before {
            return;
        }
        if !self.panel_open {
            self.seen = false;
        }
        let speaker = after.speaker().or(before.speaker()).cloned();
        let ctx = NotifyContext {
            now_ms: now.saturating_duration_since(self.epoch).as_millis() as u64,
            was_playing: self.was_playing || w.sending || w.playing_on_pc,
            panel_or_settings_in_front: self.panel_open || self.settings_open,
            tell_me: w.settings.tell_me,
            last_for_speaker: speaker.as_ref().and_then(|s| {
                self.last_notified
                    .get(s)
                    .map(|(k, at)| (*k, at.saturating_duration_since(self.epoch).as_millis() as u64))
            }),
            fell_back_at_ms: self.fell_back_at.map(|t| t.saturating_duration_since(self.epoch).as_millis() as u64),
        };
        if let Some(kind) = rules::notification_for(&before, &after, ctx) {
            self.seq += 1;
            self.notification = Some(NotificationFacts { seq: self.seq, kind, speaker: speaker.clone() });
            if let Some(s) = speaker {
                self.last_notified.insert(s, (kind, now));
            }
            out.push(Do::Notify);
        }
    }
}

#[cfg(test)]
mod tests;
