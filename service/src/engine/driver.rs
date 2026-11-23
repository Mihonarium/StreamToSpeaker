//! The engine's thread: takes commands and events in order, runs the
//! core's decisions against the app and Windows, and publishes the
//! snapshot when it changes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use log::{debug, info, warn};

use super::cancel;
use super::core::{Core, Do, Ended, Event, Job, Origin, Settings, World};
use super::facts;
use crate::app::{place_of, App};
use crate::model::command::{
    ticket, Command, DelayOutcome, DelayStep, Effect, NoticeKind, Open, Outcome, Refusal, Reply, Surface, Ticket,
    VolumeOwner,
};
use crate::model::facts::{ComesBack, ConnectFailure};
use crate::model::port::{Port, PortCore, Watch, Watcher};
use crate::model::rules::PlayedDetector;
use crate::model::state::{ConnectStage, Place, Secret};
use crate::model::text::LocalTime;
use crate::model::{build, Snapshot};
use crate::platform::{Platform, SystemEvent, ALL_ROLES, PLAYBACK_ROLES};
use crate::user_config::AutoConnect;

/// How often the engine looks at the world when nothing else happens.
const LOOK_EVERY: Duration = Duration::from_millis(250);
/// The speaker list is rebuilt at most this often.
const LIST_EVERY: Duration = Duration::from_secs(1);
/// A level is remembered for its speaker once it has held this long.
const LEVEL_SETTLES: Duration = Duration::from_secs(2);

pub(crate) enum Input {
    Command(Surface, Command, Reply),
    Event(Event),
    System(SystemEvent),
    /// End the engine's thread (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    Stop,
}

/// Where navigation commands (open a window or a page) go.
pub type Opener = Box<dyn Fn(Open) + Send + Sync>;

/// The real engine: implements the model's [`Port`].
pub struct Engine {
    port: Arc<PortCore>,
    tx: Sender<Input>,
    platform: Platform,
    opener: Arc<Mutex<Option<Opener>>>,
}

impl Engine {
    /// Start the engine for `app` on `platform`. From then on today's entry
    /// points (window, tray, web API, terminal prompt) send it commands.
    /// `go_back`: bring the remembered place back by the start rule (not
    /// when the command line or the terminal names one).
    pub fn start(app: Arc<App>, platform: Platform, go_back: bool) -> Arc<Engine> {
        let now = Instant::now();
        let (choice, named, after_update) = {
            let mut uc = app.user_config.lock().unwrap();
            let named = uc.last_speaker_id.as_deref().map(place_of);
            let choice = if uc.chose_this_pc { Some(Place::ThisPc) } else { named.clone() };
            let after_update = std::mem::take(&mut uc.closed_for_update);
            if after_update {
                uc.save();
            }
            (choice, named, after_update)
        };
        let core = Core::new(choice, named, now);
        let opener: Arc<Mutex<Option<Opener>>> = Arc::new(Mutex::new(None));
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut driver = Driver::new(app.clone(), platform.clone(), core, tx.clone(), opener.clone(), now);
        let first = driver.snapshot(1);
        let port = Arc::new(PortCore::new(first));
        driver.last_facts = None;
        let thread_port = port.clone();
        let spawned = std::thread::Builder::new().name("stream-to-speaker-engine".into()).spawn(move || {
            driver.run(rx, &thread_port, go_back.then_some(after_update));
        });
        if let Err(e) = spawned {
            warn!("the engine couldn't start: {}", e);
        }
        let events = tx.clone();
        platform.events.subscribe(Arc::new(move |e| {
            let _ = events.send(Input::System(e));
        }));
        let sink = tx.clone();
        app.set_command_sink(Box::new(move |from, cmd| {
            let (reply, ticket) = ticket();
            let _ = sink.send(Input::Command(from, cmd, reply));
            ticket
        }));
        Arc::new(Engine { port, tx, platform, opener })
    }

    /// Where navigation commands go (the window that opens Settings, a
    /// dialog or one of Windows' pages).
    pub fn on_open(&self, opener: Opener) {
        *self.opener.lock().unwrap() = Some(opener);
    }

    /// Quitting: give Windows' output back and let the speaker go, then
    /// name the app's output for when it isn't running. Waits at most a
    /// second for the engine.
    pub fn quit(&self) {
        let ticket = self.send(Surface::Panel, Command::Quit);
        let _ = ticket.wait(Duration::from_secs(1));
        let lang = self.port.snapshot().lang;
        let _ = self.platform.device.rename(&crate::model::surfaces::device_name_not_running(lang));
    }
}

#[cfg(test)]
impl Engine {
    /// End the engine's thread.
    pub(crate) fn stop(&self) {
        let _ = self.tx.send(Input::Stop);
    }
}

impl Port for Engine {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.port.snapshot()
    }

    fn watch(&self, watcher: Watcher) -> Watch {
        self.port.watch(watcher)
    }

    fn send(&self, from: Surface, command: Command) -> Ticket {
        let (reply, ticket) = ticket();
        let _ = self.tx.send(Input::Command(from, command, reply));
        ticket
    }

    fn level(&self) -> f32 {
        self.port.level()
    }
}

/// Where a command from `from` comes from: only a person picking in the
/// app's own window may choose a receiver that announces another
/// speaker's identity, and only there can a code or password be asked for.
pub(crate) fn origin_of(from: Surface) -> Origin {
    match from {
        Surface::Panel | Surface::Settings => Origin::Window,
        Surface::Tray
        | Surface::Shortcut
        | Surface::Notification
        | Surface::Phone
        | Surface::CommandLine
        | Surface::Api => Origin::Elsewhere,
    }
}

pub(crate) struct Driver {
    app: Arc<App>,
    platform: Platform,
    pub(crate) core: Core,
    tx: Sender<Input>,
    opener: Arc<Mutex<Option<Opener>>>,
    /// The connect wanted now; a connect thread whose number differs stops.
    wanted: Arc<AtomicU64>,
    detector: PlayedDetector,
    /// This PC's sound before this counts as played since the mark.
    sound_mark: Option<Duration>,
    started: Instant,
    list: (Instant, Vec<crate::model::facts::SpeakerFacts>),
    pub(crate) last_facts: Option<crate::model::Facts>,
    version: u64,
    device_name: Option<String>,
    handback: Option<(Option<crate::model::PcOutputId>, String)>,
    level: Option<(String, u8, Instant)>,
    /// Connect and release threads still running.
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Driver {
    pub(crate) fn new(
        app: Arc<App>,
        platform: Platform,
        core: Core,
        tx: Sender<Input>,
        opener: Arc<Mutex<Option<Opener>>>,
        now: Instant,
    ) -> Self {
        Driver {
            app,
            platform,
            core,
            tx,
            opener,
            wanted: Arc::new(AtomicU64::new(0)),
            detector: PlayedDetector::new(),
            sound_mark: None,
            started: now,
            list: (now - LIST_EVERY * 2, Vec::new()),
            last_facts: None,
            version: 1,
            device_name: None,
            handback: None,
            level: None,
            threads: Vec::new(),
        }
    }

    /// Wait for every connect and release thread started so far to end.
    #[cfg(test)]
    pub(crate) fn join_threads(&mut self) {
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }

    fn run(&mut self, rx: Receiver<Input>, port: &PortCore, start: Option<bool>) {
        if let Some(after_update) = start {
            let w = self.world(Instant::now());
            let dos = self.core.event(Event::Started { after_update }, &w, Instant::now());
            self.execute(dos);
        }
        loop {
            match rx.recv_timeout(LOOK_EVERY) {
                Ok(Input::Stop) => return,
                Ok(input) => self.input(input, port),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.look();
            self.publish(port);
        }
    }

    pub(crate) fn input(&mut self, input: Input, port: &PortCore) {
        let now = Instant::now();
        match input {
            Input::Command(from, cmd, reply) => {
                let result = self.command(from, &cmd, now);
                let snapshot = self.publish(port);
                reply.send(Outcome { result, snapshot });
            }
            Input::Event(ev) => {
                let w = self.world(now);
                let dos = self.core.event(ev, &w, now);
                self.execute(dos);
            }
            Input::Stop => {}
            Input::System(e) => {
                let ev = match e {
                    SystemEvent::DefaultOutputChanged => Some(Event::DefaultChanged),
                    SystemEvent::Suspending => Some(Event::Suspending),
                    SystemEvent::Resumed => Some(Event::Resumed),
                    SystemEvent::SessionEnding => Some(Event::SessionEnding),
                    SystemEvent::OutputsChanged => None,
                };
                if let Some(ev) = ev {
                    let w = self.world(now);
                    let dos = self.core.event(ev, &w, now);
                    self.execute(dos);
                }
            }
        }
    }

    /// One command from `from`.
    pub(crate) fn command(&mut self, from: Surface, cmd: &Command, now: Instant) -> Result<Effect, Refusal> {
        let app = self.app.clone();
        match cmd {
            Command::Play(Place::Speaker(id)) | Command::PlayTakingOver(id) => {
                // A receiver announcing another speaker's identity plays
                // only when a person picks it in the app's own window.
                if origin_of(from) != Origin::Window && app.is_airplay_newcomer(id.as_str()) {
                    info!("refused a pick of {} from {:?}: it announces another speaker's identity", id.as_str(), from);
                    return Err(Refusal::OnlyFromWindow { speaker: id.clone() });
                }
                self.core_command(cmd, origin_of(from), now)
            }
            Command::SetVolume { owner, level } => {
                self.set_volume(owner, Some(*level), None);
                Ok(Effect::Done)
            }
            Command::SetMuted { owner, muted } => {
                self.set_volume(owner, None, Some(*muted));
                Ok(Effect::Done)
            }
            Command::ChangeDelay { speaker, step } => {
                let request = match *step {
                    DelayStep::Remove(ms) => crate::delay::Request::Remove(ms),
                    DelayStep::Add(ms) => crate::delay::Request::Add(ms),
                    DelayStep::SetTotal(total) => crate::delay::Request::Set(total),
                    DelayStep::Reset => crate::delay::Request::Set(0),
                };
                match app.change_delay(Some(speaker.as_str()), request) {
                    Ok(o) => Ok(Effect::Delay(DelayOutcome {
                        speaker: speaker.clone(),
                        step: *step,
                        before_ms: o.from_ms,
                        after_ms: o.to_ms,
                        stopped_at_limit: o.clamped,
                    })),
                    Err(why) => Err(refusal(why, speaker)),
                }
            }
            Command::UseDelayChoice { speaker, choice } => {
                let choice = match choice {
                    crate::model::facts::DelayChoice::Short => crate::delay::DelayChoice::Short,
                    crate::model::facts::DelayChoice::Steady => crate::delay::DelayChoice::Steady,
                };
                app.set_delay_choice(speaker.as_str(), choice).map(|_| Effect::Done).map_err(|why| refusal(why, speaker))
            }
            Command::Reconnect(id) => {
                if self.core.bound() == Some(&Place::Speaker(id.clone())) {
                    app.reconnect_speaker();
                }
                Ok(Effect::Done)
            }
            Command::HideFromList(id) => {
                let mut settings = app.speaker_settings(id.as_str());
                settings.hidden = true;
                app.set_speaker_settings(id.as_str(), settings);
                self.list.0 = now - LIST_EVERY * 2;
                Ok(Effect::Done)
            }
            Command::DismissNotice(NoticeKind::Donation) => {
                app.snooze_donation_prompt(30);
                Ok(Effect::Done)
            }
            Command::Open(o) => {
                if let Some(open) = self.opener.lock().unwrap().as_ref() {
                    open(o.clone());
                }
                Ok(Effect::Done)
            }
            Command::Release { for_update } => {
                if *for_update {
                    let mut uc = app.user_config.lock().unwrap();
                    uc.closed_for_update = true;
                    uc.save();
                }
                self.core_command(cmd, origin_of(from), now)
            }
            Command::PlayTestSound(_)
            | Command::AllowDevice { .. }
            | Command::TurnOffLab
            | Command::DismissNotice(_)
            | Command::FirstRun(_) => Ok(Effect::Done),
            _ => self.core_command(cmd, origin_of(from), now),
        }
    }

    fn core_command(&mut self, cmd: &Command, origin: Origin, now: Instant) -> Result<Effect, Refusal> {
        let w = self.world(now);
        let (result, dos) = self.core.command(cmd, origin, &w, now);
        self.execute(dos);
        result
    }

    fn set_volume(&mut self, owner: &VolumeOwner, level: Option<u8>, muted: Option<bool>) {
        match owner {
            VolumeOwner::Speaker(id) => {
                if self.core.bound() != Some(&Place::Speaker(id.clone())) {
                    return;
                }
                if let Some(level) = level {
                    self.app.set_speaker_volume(level.min(100) as u32);
                }
                if let Some(muted) = muted {
                    self.app.set_speaker_muted(muted);
                }
            }
            VolumeOwner::PcOutput(id) => {
                let (old, was_muted) = self.platform.outputs.volume(id).unwrap_or((0, false));
                if let Err(e) =
                    self.platform.outputs.set_volume(id, level.unwrap_or(old), muted.unwrap_or(was_muted))
                {
                    debug!("couldn't set this PC's level: {}", e);
                }
            }
            // The level sent to Music Assistant isn't scaled yet.
            VolumeOwner::MusicAssistant => {}
        }
    }

    /// Carry out the core's decisions.
    pub(crate) fn execute(&mut self, dos: Vec<Do>) {
        self.threads.retain(|t| !t.is_finished());
        for d in dos {
            match d {
                Do::Connect(job) => self.connect(job),
                Do::Supersede => self.wanted.store(0, Ordering::SeqCst),
                // Quitting: the app's own teardown stops the speaker and
                // waits for it to answer, as before.
                Do::Release { .. } if self.app.is_shutting_down() => {}
                Do::Release { tell_speaker } => self.threads.extend(self.app.release_session(tell_speaker)),
                Do::UseOurOutput => {
                    let Some(ours) = self.platform.outputs.our_output() else { continue };
                    if let Err(e) = self.platform.outputs.set_default(&ours, PLAYBACK_ROLES) {
                        warn!("couldn't make this app's output Windows' default: {}", e);
                    }
                    let ok = self.platform.outputs.default_output().as_ref() == Some(&ours);
                    let _ = self.tx.send(Input::Event(Event::SwitchRead { ok }));
                }
                Do::GiveBack => {
                    let ours = self.platform.outputs.our_output();
                    if ours.is_some() && self.platform.outputs.default_output() == ours {
                        if let Some(pc) = self.core.pc_output.clone() {
                            if let Err(e) = self.platform.outputs.set_default(&pc, ALL_ROLES) {
                                warn!("couldn't give Windows' output back: {}", e);
                            }
                        }
                    }
                }
                Do::SaveChoice(place) => {
                    let mut uc = self.app.user_config.lock().unwrap();
                    match place {
                        Some(Place::Speaker(id)) => {
                            // As before: not a receiver announcing another
                            // speaker's identity, so that the next start
                            // never connects to it by itself.
                            let id = id.as_str();
                            if !self.app.is_airplay_newcomer(id) && !crate::airplay::discovery::is_instance_id(id) {
                                uc.last_speaker_id = Some(id.to_string());
                            }
                            uc.chose_this_pc = false;
                        }
                        Some(Place::MusicAssistant) => {
                            uc.last_speaker_id = Some(crate::sendspin_app::MA_OUTPUT_ID.to_string());
                            uc.chose_this_pc = false;
                        }
                        Some(Place::ThisPc) => uc.chose_this_pc = true,
                        None => {}
                    }
                    uc.save();
                }
                Do::ResetPlayed => {
                    self.detector.reset();
                    self.sound_mark = Some(self.app.last_sound_at().unwrap_or_default());
                }
                Do::Notify => {}
            }
        }
    }

    /// Start a connect on a thread of its own; it ends with the connect.
    fn connect(&mut self, job: Job) {
        self.wanted.store(job.gen, Ordering::SeqCst);
        let id = connect_id(&job.place);
        if let Some(id) = &id {
            self.app.engine_connect_started(id, job.gen);
        }
        if let Place::Speaker(id) = &job.place {
            // Only a person's pick in the window may play to a receiver
            // announcing another speaker's identity: not a return, a retry,
            // start-up or Windows choosing the app's output.
            if job.origin != Origin::Window && self.app.is_airplay_newcomer(id.as_str()) {
                info!("not connecting to {} by itself: it announces another speaker's identity", id.as_str());
                let words = format!(
                    "To play to “{}”, pick it in the Stream To Speaker window.",
                    self.app.speaker_name(id.as_str()).unwrap_or_else(|| id.as_str().to_string())
                );
                self.app.engine_connect_ended(id.as_str(), job.gen, Err(words));
                let ended = Ended::Failed(ConnectFailure::Other);
                let _ = self.tx.send(Input::Event(Event::Ended { gen: job.gen, ended }));
                return;
            }
        }
        let app = self.app.clone();
        let tx = self.tx.clone();
        let wanted = self.wanted.clone();
        let gen = job.gen;
        let job_place = job.place.clone();
        let spawned = std::thread::Builder::new().name("stream-to-speaker-connect".into()).spawn(move || {
            let check = wanted.clone();
            let _scope = cancel::enter(Arc::new(move || check.load(Ordering::SeqCst) != gen));
            let stage_tx = tx.clone();
            let (ended, result) = connect_job(&app, &job, &move |stage| {
                let _ = stage_tx.send(Input::Event(Event::Stage { gen, stage }));
            });
            if let Some(id) = &id {
                app.engine_connect_ended(id, gen, result);
            }
            let _ = tx.send(Input::Event(Event::Ended { gen, ended }));
        });
        if let Ok(t) = spawned {
            self.threads.push(t);
        } else {
            if let Some(id) = connect_id(&job_place) {
                let name = self.app.speaker_name(&id).unwrap_or_else(|| id.clone());
                let words = format!(
                    "Stream To Speaker couldn't start connecting to {name}: this PC is short of resources. Try again in \
                     a moment."
                );
                self.app.engine_connect_ended(&id, gen, Err(words));
            }
            let _ = self.tx.send(Input::Event(Event::Ended { gen, ended: Ended::Failed(ConnectFailure::CouldntStart) }));
        }
    }

    /// What the world looks like now.
    pub(crate) fn world(&mut self, now: Instant) -> World {
        let list = self.speakers(now).clone();
        let app = &self.app;
        let outputs = &self.platform.outputs;
        let default_output = outputs.default_output();
        let our_output = outputs.our_output();
        let sessions = self.platform.sessions.sessions();
        const AUDIBLE: f32 = PlayedDetector::THRESHOLD;
        let ours_id = our_output.clone();
        let mut apps_here: Vec<String> = Vec::new();
        let mut app_elsewhere = None;
        let mut loudest: f32 = 0.0;
        for s in sessions.iter().filter(|s| !s.system_sounds && s.peak > AUDIBLE) {
            loudest = loudest.max(s.peak);
            if Some(&s.output) == ours_id.as_ref() {
                if !apps_here.contains(&s.app) {
                    apps_here.push(s.app.clone());
                }
            } else if app_elsewhere.is_none() {
                app_elsewhere = Some(s.app.clone());
            }
        }
        let sending = app.receiving_windows_audio();
        let since_start = now.saturating_duration_since(self.started).as_millis() as u64;
        let mut played_since = self.detector.observe(since_start, loudest);
        if let (Some(mark), Some(sound)) = (self.sound_mark, app.last_sound_at()) {
            played_since |= sound > mark;
        }
        let delay = app.delay_status();
        let delay_reconnect = delay.as_ref().and_then(|d| match d.progress {
            crate::delay::Progress::Reconnecting { from_ms } => Some((from_ms, d.change_ms)),
            _ => None,
        });
        let (ma_connected, ma_playing) = match app.sendspin_source_state() {
            Some(crate::sendspin::source::SourceState::Connected { streaming, .. }) => (true, streaming),
            _ => (false, false),
        };
        let found = list
            .iter()
            .filter(|s| s.presence == crate::model::facts::Presence::Found)
            .map(|s| s.id.clone())
            .collect();
        World {
            found,
            sending,
            apps_here: crate::model::state::Apps(apps_here),
            app_elsewhere: if sending { None } else { app_elsewhere },
            playing_on_pc: loudest > AUDIBLE || sending,
            played_since,
            knows_outputs: default_output.is_some(),
            default_output,
            our_output,
            delay_reconnect,
            not_taking_since: None,
            ma_connected,
            ma_playing,
            ma_room: None,
            pin_ceremony: app.pin_ceremony_id().is_some(),
            time: local_time(),
            settings: self.settings(),
        }
    }

    fn settings(&self) -> Settings {
        let uc = self.app.user_config.lock().unwrap();
        let comes_back = uc
            .speaker_settings
            .iter()
            .map(|(id, s)| {
                let c = match s.auto_connect {
                    AutoConnect::Default => ComesBack::LastChosen,
                    AutoConnect::Always => ComesBack::WheneverAppears,
                    AutoConnect::Never => ComesBack::Never,
                };
                (crate::model::SpeakerId::new(id), c)
            })
            .collect();
        Settings {
            stay_silent: !uc.auto_reconnect_on_drop,
            tell_me: true,
            go_back_at_start: uc.auto_reconnect_on_launch,
            comes_back,
            sent_apps_mode: false,
        }
    }

    fn speakers(&mut self, now: Instant) -> &Vec<crate::model::facts::SpeakerFacts> {
        if now.saturating_duration_since(self.list.0) >= LIST_EVERY {
            self.list = (now, facts::speaker_list(&self.app));
        }
        &self.list.1
    }

    /// What the session says by itself: it dropped, someone else took it,
    /// the speaker re-buffers; and a session bound by another path.
    pub(crate) fn look(&mut self) {
        let now = Instant::now();
        let app = self.app.clone();
        let reconnecting_for_delay = app
            .delay_status()
            .is_some_and(|d| matches!(d.progress, crate::delay::Progress::Reconnecting { .. }));
        let info = {
            let guard = app.session.lock().unwrap();
            guard.as_ref().map(|s| (s.stable_id(), s.is_dead(), s.final_fault(), s.ended_by_speaker(), upnp_info(s)))
        };
        let mut events = Vec::new();
        match (&info, self.core.bound().cloned()) {
            (Some((id, dead, fault, ended, _)), Some(bound)) if place_of(id) == bound => {
                match ended.as_deref() {
                    // The speaker went to another source, or a person
                    // stopped it there: leave it alone.
                    Some("another_server" | "user_request") => events.push(Event::Taken),
                    Some(_) => events.push(Event::Dropped),
                    // A fault no reconnect can fix (the receiver refused
                    // our pairing): straight back to this PC.
                    None if *dead && fault.is_some() => events.push(Event::PairingRefused),
                    None if *dead && !reconnecting_for_delay => events.push(Event::Dropped),
                    None => {}
                }
            }
            _ => {}
        }
        if let Some(change) = app.take_transport_change() {
            let state = change.state.as_deref().unwrap_or_default().to_ascii_uppercase();
            if state == "TRANSITIONING" {
                app.delay.catch_up().speaker_rebuffered();
            }
            if let (Some((_, _, _, _, Some(ours))), Some(uri)) = (&info, change.uri.as_deref()) {
                if crate::upnp::is_busy(&state, uri, ours) || (state.is_empty() && !uri.is_empty() && !uri.starts_with(ours.as_str())) {
                    events.push(Event::Taken);
                }
            }
        }
        let w = self.world(now);
        for ev in events {
            let dos = self.core.event(ev, &w, now);
            self.execute(dos);
        }
        // A session bound or ended by another path (a delay change's
        // reconnect, a Sendspin pairing answer) is taken as it is.
        let app_place = app.session.lock().unwrap().as_ref().map(|s| place_of(&s.stable_id()));
        if self.core.connecting().is_none() && app_place.as_ref() != self.core.bound() {
            self.core.adopt(app_place);
        }
        let dos = self.core.tick(&w, now);
        self.execute(dos);
        self.remember_level(now);
    }

    /// Remember the bound speaker's level once it has settled.
    fn remember_level(&mut self, now: Instant) {
        let (Some(Place::Speaker(id)), Some(level)) = (self.core.bound().cloned(), self.app.vsync.current_level())
        else {
            self.level = None;
            return;
        };
        let level = level.min(100) as u8;
        match &self.level {
            Some((i, l, since)) if i == id.as_str() && *l == level => {
                if now.saturating_duration_since(*since) >= LEVEL_SETTLES {
                    let mut s = self.app.speaker_settings(id.as_str());
                    if s.level != Some(level) {
                        s.level = Some(level);
                        self.app.set_speaker_settings(id.as_str(), s);
                    }
                }
            }
            _ => self.level = Some((id.as_str().to_string(), level, now)),
        }
    }

    /// The snapshot for now.
    pub(crate) fn snapshot(&mut self, version: u64) -> Snapshot {
        let now = Instant::now();
        let w = self.world(now);
        let facts = facts::facts(&self.app, &self.platform, &self.core, &w, now, self.started, self.list.1.clone());
        let snapshot = build(&facts, version);
        self.last_facts = Some(facts);
        snapshot
    }

    /// Publish the snapshot if anything changed; returns the current one.
    pub(crate) fn publish(&mut self, port: &PortCore) -> Arc<Snapshot> {
        let now = Instant::now();
        let w = self.world(now);
        let facts = facts::facts(&self.app, &self.platform, &self.core, &w, now, self.started, self.list.1.clone());
        let peak = self.platform.outputs.our_output().map(|o| self.platform.sessions.peak(&o)).unwrap_or(0.0);
        port.set_level(if w.sending { peak.max(0.05) } else { 0.0 });
        if self.last_facts.as_ref() == Some(&facts) {
            return port.snapshot();
        }
        self.version = port.next_version();
        let snapshot = build(&facts, self.version);
        self.last_facts = Some(facts);
        self.follow_names(&snapshot);
        port.publish(snapshot)
    }

    /// Windows' name for the app's output, and what the app does if it
    /// dies, follow the snapshot.
    fn follow_names(&mut self, snapshot: &Snapshot) {
        if snapshot.device_name != self.device_name {
            if let Some(name) = &snapshot.device_name {
                if let Err(e) = self.platform.device.rename(name) {
                    debug!("couldn't name the app's output: {}", e);
                }
            }
            self.device_name = snapshot.device_name.clone();
        }
        let handback = (
            self.core.pc_output.clone(),
            crate::model::surfaces::device_name_not_running(snapshot.lang),
        );
        if self.handback.as_ref() != Some(&handback) {
            self.platform.handback.prepare(handback.0.as_ref(), &handback.1);
            self.handback = Some(handback);
        }
    }
}

/// For a UPnP session: the start of this PC's stream addresses toward the
/// speaker (`http://<address>:<port>/`).
fn upnp_info(s: &crate::app::ActiveSession) -> Option<String> {
    match s {
        crate::app::ActiveSession::Upnp(r) => {
            let uri = &r.stream_uri;
            let rest = uri.strip_prefix("http://")?;
            let host_end = rest.find('/')?;
            Some(format!("http://{}/", &rest[..host_end]))
        }
        _ => None,
    }
}

fn refusal(why: crate::delay::Unchangeable, speaker: &crate::model::SpeakerId) -> Refusal {
    match why {
        crate::delay::Unchangeable::NoSpeaker => Refusal::NoDelayHere,
        crate::delay::Unchangeable::MusicAssistant => Refusal::MusicAssistantDelay,
        crate::delay::Unchangeable::KeptBySpeakerOnSteady => Refusal::FixedOnSteady { speaker: speaker.clone() },
    }
}

/// The app's id for a place a connect goes to.
fn connect_id(place: &Place) -> Option<String> {
    match place {
        Place::Speaker(id) => Some(id.as_str().to_string()),
        Place::MusicAssistant => Some(crate::sendspin_app::MA_OUTPUT_ID.to_string()),
        Place::ThisPc => None,
    }
}

/// Run one connect on this thread: how it ended, for the engine and in
/// words for a caller waiting on it.
fn connect_job(app: &Arc<App>, job: &Job, stage: &dyn Fn(ConnectStage)) -> (Ended, Result<(), String>) {
    let Some(id) = connect_id(&job.place) else { return (Ended::Cancelled, Ok(())) };
    let superseded = || {
        let name = app.speaker_name(&id).unwrap_or_else(|| id.clone());
        (Ended::Cancelled, Err(format!("Stopped connecting to {name}: something else was chosen meanwhile.")))
    };
    if job.check_busy {
        if let Some(other_pc) = busy(app, &id) {
            let lang = crate::model::text::Lang::English;
            let name = app.speaker_name(&id).unwrap_or_else(|| id.clone());
            let words = match &other_pc {
                Some(pc) => crate::model::text::t!(lang, BusyOtherPcTitle, speaker = name.as_str(), pc = pc.as_str()),
                None => crate::model::text::t!(lang, BusyTitle, speaker = name.as_str()),
            };
            return (Ended::Busy { other_pc }, Err(words));
        }
    }
    if cancel::cancelled() {
        return superseded();
    }
    stage(ConnectStage::Asking);
    match app.engine_connect(&id, job.origin == Origin::Window, job.origin != Origin::ByItself) {
        Ok(()) if cancel::cancelled() => superseded(),
        Ok(()) => (Ended::Bound, Ok(())),
        Err(msg) => {
            let ended = ended_by(app, &id, job, &msg);
            if ended == Ended::Cancelled {
                return superseded();
            }
            (ended, Err(msg))
        }
    }
}

/// How a connect that failed with `msg` ended.
fn ended_by(app: &App, id: &str, job: &Job, msg: &str) -> Ended {
    if msg == crate::app::SUPERSEDED || cancel::cancelled() {
        return Ended::Cancelled;
    }
    let window = job.origin == Origin::Window;
    if window && app.pin_ceremony_id().as_deref() == Some(id) {
        return Ended::Pairing;
    }
    if crate::app::needs_the_person(msg) {
        return if window { Ended::Failed(ConnectFailure::Other) } else { Ended::NeedsSecret(Secret::Code) };
    }
    if app.asked_for_password(id) {
        return if window {
            Ended::Failed(ConnectFailure::WrongPassword)
        } else {
            Ended::NeedsSecret(Secret::Password)
        };
    }
    let lower = msg.to_lowercase();
    if lower.contains("busy") {
        return Ended::Busy { other_pc: None };
    }
    if lower.contains("no thread") {
        return Ended::Failed(ConnectFailure::CouldntStart);
    }
    let no_answer = ["timed out", "too slow", "refused", "unreachable", "no route", "not found", "10060", "10061"];
    if no_answer.iter().any(|w| lower.contains(w)) {
        return Ended::Failed(ConnectFailure::NoAnswer);
    }
    Ended::Failed(ConnectFailure::Other)
}

/// Whether the UPnP speaker `id` plays something else now (asked before a
/// connect); `Some(other PC)` when it does. Speakers that can't say, and
/// any error, count as free.
fn busy(app: &App, id: &str) -> Option<Option<String>> {
    let r = app.discovery.as_ref()?.find_by_id(id)?;
    let local = app.local_ip_toward(r.ip).ok()?;
    let ours = format!("http://{}/", std::net::SocketAddr::new(local, app.config.bind.port()));
    let state = crate::upnp::get_transport_state(&r.av_transport_control_url).ok()?;
    let (uri, title) = crate::upnp::get_media_info(&r.av_transport_control_url).ok()?;
    crate::upnp::is_busy(&state, &uri, &ours).then(|| title.as_deref().and_then(crate::upnp::other_pc_in_title))
}

/// The time on the PC's clock.
pub(crate) fn local_time() -> LocalTime {
    #[cfg(windows)]
    {
        // SAFETY: GetLocalTime only writes the SYSTEMTIME it is given.
        let st = unsafe {
            let mut st = std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>();
            windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st);
            st
        };
        LocalTime::new(st.wHour as u8, st.wMinute as u8)
    }
    #[cfg(not(windows))]
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        LocalTime::new(((secs / 3600) % 24) as u8, ((secs / 60) % 60) as u8)
    }
}

#[cfg(test)]
mod tests;
