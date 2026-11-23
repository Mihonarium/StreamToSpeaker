//! The running part of a speaker's delay: the status every surface shows,
//! the acts that change it, and the thread that applies and keeps the
//! changes.
//!
//! The acts return at once. A change is recorded in the settings in
//! memory and handed to the engine's own thread, which saves it and, on a
//! speaker whose change applies by reconnecting, reconnects it (see
//! [`super::reconnect`]); on a UPnP speaker the stream itself catches up
//! (see [`super::catch_up`]).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use log::{info, warn};

use super::catch_up::{BackOff, CatchUp};
use super::reconnect::Schedule;
use super::{setting, Applies, Bases, DelayChoice, Kind, Limits, Live, Request, Setting, Unchangeable};
use crate::airplay::AirPlayRenderer;
use crate::app::{ActiveSession, App};

/// What surfaces show about one speaker's delay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelayStatus {
    /// The speaker's id and name.
    pub id: String,
    pub name: String,
    pub kind: Kind,
    /// Short delay or Steady, for an AirPlay speaker or a single HomePod.
    pub choice: Option<DelayChoice>,
    /// The speaker's delay as far as the app knows it, ms, change
    /// included.
    pub delay_ms: u32,
    /// Its own delay, with nothing removed or added, ms.
    pub own_delay_ms: u32,
    /// What was removed (negative) or added (positive), ms.
    pub change_ms: i32,
    /// How far it can be changed; `None` where it can't be.
    pub limits: Option<Limits>,
    /// How a change reaches it, or why it can't.
    pub applies: Result<Applies, Unchangeable>,
    /// Whether a change is still on its way to the speaker.
    pub progress: Progress,
    /// Something to tell about the last change, until the next one.
    pub notice: Option<Notice>,
}

/// How far the last change has reached the speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// It has what the status says (or isn't playing, and gets it when it
    /// next does).
    Done,
    /// UPnP: the stream is catching up; `ms_to_go` is negative while delay
    /// is being removed, positive while it is being added.
    CatchingUp { ms_to_go: i32 },
    /// It reconnects to apply the change: a few seconds of silence.
    /// Waits for the presses to stop, or for the reconnect before it.
    WillReconnect,
    /// It is reconnecting to apply it. Not a dropout. `from_ms`: what was
    /// removed or added until now.
    Reconnecting { from_ms: i32 },
}

/// Something to tell about a speaker's last change, until the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// The speaker didn't accept `tried_ms`, so it plays `back_to_ms`.
    NotAccepted { tried_ms: i32, back_to_ms: i32 },
    /// The speaker ran dry soon after delay was removed, so some of it
    /// went back.
    DroppedOut { from_ms: i32, to_ms: i32 },
    /// The delay choice was switched to `to`; each choice keeps its own
    /// change: `now_ms` applies, `other_ms` stays with the other choice.
    ChoiceSwitched { to: DelayChoice, now_ms: i32, other_ms: i32 },
}

/// What a change did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeOutcome {
    /// What was removed or added before the change, and after it, ms.
    pub from_ms: i32,
    pub to_ms: i32,
    /// A limit stopped it short of what was asked.
    pub clamped: bool,
    /// The speaker's status with the change.
    pub status: DelayStatus,
}

/// What an AirPlay session asks the receiver for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AirPlayDelay {
    /// The delay asked for on the usual path, ms (at least 1000 offers the
    /// AirPlay 2 buffered stream).
    pub latency_ms: u32,
    /// Play a HomePod with its short profile, at `homepod_latency_ms`.
    pub homepod_realtime: bool,
    pub homepod_latency_ms: u32,
}

/// The engine: one per app.
pub struct DelayEngine {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    catch_up: Arc<CatchUp>,
    worker: AtomicBool,
}

#[derive(Default)]
struct State {
    /// The speaker whose newest session began, and the change it began
    /// with (what that speaker has, while that session lasts).
    started: Option<(String, i32)>,
    /// The UPnP speaker the stream's catch-up is for.
    upnp: Option<String>,
    /// The speaker a reconnect waits or runs for, and its schedule.
    reconnect_for: Option<String>,
    schedule: Schedule,
    /// The reconnect that runs: for whom, from which change.
    reconnecting: Option<(String, i32)>,
    /// What to tell about a speaker's last change.
    notice: Option<(String, Notice)>,
    /// Counts the changes to the settings in memory, which the engine's
    /// thread saves; `saved` is the count it last saved.
    changes: u64,
    saved: u64,
    /// Back-offs the stream made, for the engine's thread to keep.
    back_offs: Vec<BackOff>,
}

impl DelayEngine {
    /// The engine; `step` is the catch-up's frames per packet.
    pub fn new(step: Arc<AtomicU32>) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
                catch_up: CatchUp::new(step),
                worker: AtomicBool::new(false),
            }),
        }
    }

    /// The UPnP stream's catch-up, which the stream server applies to the
    /// speaker's fetches.
    pub fn catch_up(&self) -> Arc<CatchUp> {
        self.shared.catch_up.clone()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap()
    }

    fn wake(&self) {
        self.shared.wake.notify_all();
    }

    /// Wait until the engine's thread has done what it was given (saved
    /// every change, made every reconnect), for at most `limit`.
    #[cfg(test)]
    pub(crate) fn wait_idle(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut state = self.state();
        loop {
            let busy = state.saved != state.changes
                || !state.back_offs.is_empty()
                || state.schedule.waiting()
                || state.schedule.running();
            if !busy {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self.shared.wake.wait_timeout(state, deadline - now).unwrap().0;
        }
    }
}

/// What the app knows of the speaker that plays.
struct Bound {
    id: String,
    name: String,
    kind: Kind,
    live: Live,
}

/// The kind of an AirPlay receiver, and whether on Steady it would take
/// the AirPlay 2 buffered stream: it is reached over AirPlay 2 first (it
/// needs it, or has no AirPlay 1 path) and takes that stream there.
pub fn airplay_kind(r: &AirPlayRenderer, prefer_realtime: bool) -> Kind {
    if r.pair.is_some() {
        return Kind::HomePodPair;
    }
    let over_ap2 = r.supports_airplay2() && (r.requires_airplay2() || !r.supports_legacy_raop());
    let buffered_on_steady = over_ap2 && r.expects_ptp() && r.supports_buffered_audio() && !prefer_realtime;
    if r.is_homepod() {
        Kind::HomePod { buffered_on_steady }
    } else {
        Kind::AirPlay { buffered_on_steady }
    }
}

impl App {
    /// The delay bases the settings hold.
    fn delay_bases(&self) -> Bases {
        let uc = self.user_config.lock().unwrap();
        Bases {
            steady_ms: uc.effective_airplay_latency_ms(),
            homepod_ms: uc.effective_homepod_latency_ms(),
            homepod_steady_by_default: uc.airplay_homepod_buffered,
        }
    }

    /// The speaker that plays, and what its session tells.
    fn bound_for_delay(&self) -> Option<Bound> {
        let prefer_realtime = self.user_config.lock().unwrap().prefer_realtime_airplay;
        let guard = self.session.lock().unwrap();
        let s = guard.as_ref()?;
        let (kind, live) = match s {
            ActiveSession::Upnp(_) => (Kind::Upnp, Live::default()),
            ActiveSession::AirPlay(a) => (
                airplay_kind(&a.renderer, prefer_realtime),
                Live { receiver_min_ms: a.receiver_min_ms(), buffered: Some(false), ..Default::default() },
            ),
            ActiveSession::AirPlay2(a) => (
                airplay_kind(&a.renderer, prefer_realtime),
                Live { buffered: Some(a.is_buffered()), ..Default::default() },
            ),
            ActiveSession::Sendspin(p) => {
                let (own, most) = p.delay_bounds();
                (Kind::Sendspin, Live { sendspin_own_ms: Some(own), sendspin_add_max_ms: most, ..Default::default() })
            }
            ActiveSession::MusicAssistant(_) => (Kind::MusicAssistant, Live::default()),
        };
        Some(Bound { id: s.stable_id(), name: s.friendly_name(), kind, live })
    }

    /// What kind of speaker `id` is, from its id and what discovery
    /// announced; an AirPlay speaker discovery doesn't list counts as a
    /// plain one.
    fn delay_kind(&self, id: &str) -> Kind {
        if id == crate::sendspin_app::MA_OUTPUT_ID {
            Kind::MusicAssistant
        } else if crate::sendspin_app::is_sendspin_id(id) {
            Kind::Sendspin
        } else if id.starts_with("airplay:pair:") {
            Kind::HomePodPair
        } else if id.starts_with("airplay:") {
            let prefer_realtime = self.user_config.lock().unwrap().prefer_realtime_airplay;
            match self.airplay_discovery.as_deref().and_then(|d| d.find_by_id(id)) {
                Some(r) => {
                    self.settle_airplay_delay(&r);
                    airplay_kind(&r, prefer_realtime)
                }
                None => Kind::AirPlay { buffered_on_steady: false },
            }
        } else {
            Kind::Upnp
        }
    }

    /// Move an earlier version's buffer for `r` into its delay choice and
    /// change, if that is still to do.
    fn settle_airplay_delay(&self, r: &AirPlayRenderer) {
        let moved = self.user_config.lock().unwrap().settle_airplay_delay(&r.stable_id(), r.is_homepod());
        if moved {
            self.delay.state().changes += 1;
            self.delay.wake();
        }
    }

    /// `id`'s delay as its settings make it, with `live` from its session.
    fn delay_setting(&self, id: &str, kind: Kind, live: &Live) -> Setting {
        let bases = self.delay_bases();
        let stored = self.user_config.lock().unwrap().speaker_settings.get(id).cloned().unwrap_or_default();
        setting(kind, &stored, &bases, live)
    }

    /// The delay of the speaker that plays; `None` while sound plays on
    /// this PC.
    pub fn delay_status(&self) -> Option<DelayStatus> {
        let bound = self.bound_for_delay()?;
        Some(self.status_with(&bound.id, bound.name.clone(), bound.kind, &bound.live, true))
    }

    /// The delay of speaker `id`, whether or not it plays.
    pub fn delay_status_of(&self, id: &str) -> DelayStatus {
        match self.bound_for_delay().filter(|b| b.id == id) {
            Some(b) => self.status_with(id, b.name.clone(), b.kind, &b.live, true),
            None => {
                let name = self.speaker_name(id).unwrap_or_else(|| id.to_string());
                self.status_with(id, name, self.delay_kind(id), &Live::default(), false)
            }
        }
    }

    fn status_with(&self, id: &str, name: String, kind: Kind, live: &Live, playing: bool) -> DelayStatus {
        let s = self.delay_setting(id, kind, live);
        let state = self.delay.state();
        let progress = match s.applies {
            Ok(Applies::Live) if playing && state.upnp.as_deref() == Some(id) => {
                match self.delay.shared.catch_up.ms_to_go() {
                    0 => Progress::Done,
                    ms_to_go => Progress::CatchingUp { ms_to_go },
                }
            }
            Ok(Applies::Reconnect) => match &state.reconnecting {
                Some((rid, from_ms)) if rid == id => Progress::Reconnecting { from_ms: *from_ms },
                _ if state.reconnect_for.as_deref() == Some(id) && state.schedule.waiting() => Progress::WillReconnect,
                _ => Progress::Done,
            },
            _ => Progress::Done,
        };
        let notice = state.notice.as_ref().filter(|(nid, _)| nid == id).map(|(_, n)| *n);
        DelayStatus {
            id: id.to_string(),
            name,
            kind,
            choice: s.choice,
            delay_ms: s.delay_ms(),
            own_delay_ms: s.own_ms,
            change_ms: s.change_ms,
            limits: s.limits,
            applies: s.applies,
            progress,
            notice,
        }
    }

    /// Remove delay from speaker `id` (the one that plays when `None`),
    /// add some, or set exactly what is removed or added. Returns at once
    /// with what was done; the change is saved and applied by the
    /// engine's thread (on a UPnP speaker the stream catches up; elsewhere
    /// the speaker reconnects a moment after the last press). A press past
    /// a limit stops at it (`clamped`).
    pub fn change_delay(self: &Arc<Self>, id: Option<&str>, request: Request) -> Result<ChangeOutcome, Unchangeable> {
        let status = match id {
            Some(id) => self.delay_status_of(id),
            None => self.delay_status().ok_or(Unchangeable::NoSpeaker)?,
        };
        let id = status.id.clone();
        let playing = self.bound_for_delay().is_some_and(|b| b.id == id);
        let s = self.delay_setting(&id, status.kind, &self.live_for(&id));
        let (to_ms, clamped) = s.after(request).ok_or_else(|| s.applies.err().unwrap_or(Unchangeable::NoSpeaker))?;
        let from_ms = s.change_ms;
        {
            let mut uc = self.user_config.lock().unwrap();
            let mut entry = uc.speaker_settings.remove(&id).unwrap_or_default();
            super::store_change(&mut entry, s.choice, to_ms);
            if !entry.is_default() {
                uc.speaker_settings.insert(id.clone(), entry);
            }
        }
        info!("delay: {} now {:+} ms (was {:+} ms{})", id, to_ms, from_ms, if clamped { ", at its limit" } else { "" });
        {
            let mut state = self.delay.state();
            state.changes += 1;
            state.notice = None;
            if playing {
                match s.applies {
                    Ok(Applies::Live) => self.delay.shared.catch_up.set_target(to_ms),
                    Ok(Applies::Reconnect) => {
                        state.reconnect_for = Some(id.clone());
                        state.schedule.press(Instant::now());
                    }
                    Err(_) => {}
                }
            }
        }
        self.delay_worker();
        Ok(ChangeOutcome { from_ms, to_ms, clamped, status: self.delay_status_of(&id) })
    }

    /// Short delay or Steady for speaker `id` (an AirPlay speaker or a
    /// single HomePod). Each keeps its own change. A speaker that plays
    /// reconnects to apply it. Returns at once.
    pub fn set_delay_choice(self: &Arc<Self>, id: &str, choice: DelayChoice) -> Result<DelayStatus, Unchangeable> {
        let before = self.delay_status_of(id);
        if before.choice.is_none() {
            return Err(before.applies.err().unwrap_or(Unchangeable::NoSpeaker));
        }
        if before.choice == Some(choice) {
            return Ok(before);
        }
        let (now_ms, other_ms) = {
            let mut uc = self.user_config.lock().unwrap();
            let entry = uc.speaker_settings.entry(id.to_string()).or_default();
            entry.delay_choice = Some(choice);
            let other = if choice == DelayChoice::Short { DelayChoice::Steady } else { DelayChoice::Short };
            (super::stored_change(entry, Some(choice)), super::stored_change(entry, Some(other)))
        };
        let playing = self.bound_for_delay().is_some_and(|b| b.id == id);
        {
            let mut state = self.delay.state();
            state.changes += 1;
            state.notice = Some((id.to_string(), Notice::ChoiceSwitched { to: choice, now_ms, other_ms }));
            if playing {
                state.reconnect_for = Some(id.to_string());
                state.schedule.press(Instant::now());
            }
        }
        self.delay_worker();
        Ok(self.delay_status_of(id))
    }

    /// Connect the speaker that plays again, keeping its delay (what
    /// Resync was). Returns at once; the reconnect runs on a thread of its
    /// own and says on failure what went wrong.
    pub fn reconnect_speaker(self: &Arc<Self>) {
        let app = self.clone();
        let spawned = std::thread::Builder::new().name("stream-to-speaker-reconnect".into()).spawn(move || {
            if let Err(e) = app.resync() {
                app.record_error(format!("Couldn't reconnect: {}", e));
            }
        });
        if spawned.is_err() {
            self.record_error("Couldn't reconnect (no thread available).");
        }
    }

    /// What the session of `id` tells, if it plays.
    fn live_for(&self, id: &str) -> Live {
        self.bound_for_delay().filter(|b| b.id == id).map(|b| b.live).unwrap_or_default()
    }

    // -------------------------------------------------------------------
    // Today's controls: a positive number removes, a negative one adds
    // -------------------------------------------------------------------

    /// Remove `ms` of the playing speaker's delay (a negative `ms` adds
    /// that much). Returns what is still to apply, in frames: positive
    /// while delay is being removed.
    pub fn adjust_latency(self: &Arc<Self>, ms: i32) -> i64 {
        let request = if ms >= 0 { Request::Remove(ms as u32) } else { Request::Add(ms.unsigned_abs()) };
        let _ = self.change_delay(None, request);
        super::ms_to_frames(self.pending_latency_ms() as i64)
    }

    /// What of the playing speaker's last change is still to apply, ms:
    /// positive while delay is being removed, negative while added.
    pub fn pending_latency_ms(&self) -> i32 {
        match self.delay_status() {
            Some(DelayStatus { progress: Progress::CatchingUp { ms_to_go }, .. }) => -ms_to_go,
            Some(DelayStatus { progress: Progress::WillReconnect, change_ms, .. }) => {
                let from = self.delay.state().started.as_ref().map_or(0, |(_, c)| *c);
                from - change_ms
            }
            Some(DelayStatus { progress: Progress::Reconnecting { from_ms }, change_ms, .. }) => from_ms - change_ms,
            _ => 0,
        }
    }

    // -------------------------------------------------------------------
    // What sessions ask as they start
    // -------------------------------------------------------------------

    /// A UPnP session to `id` begins: the stream's catch-up starts over
    /// with its change, before the speaker's first fetch.
    pub(crate) fn delay_for_upnp(&self, id: &str) {
        let change = self.delay_setting(id, Kind::Upnp, &Live::default()).change_ms;
        let mut state = self.delay.state();
        state.upnp = Some(id.to_string());
        state.started = Some((id.to_string(), change));
        self.delay.shared.catch_up.begin_session(change);
    }

    /// What an AirPlay session to `r` asks for (its delay choice and
    /// change), noted as what it starts with.
    pub(crate) fn delay_for_airplay(&self, r: &AirPlayRenderer) -> AirPlayDelay {
        let id = r.stable_id();
        let prefer_realtime = self.user_config.lock().unwrap().prefer_realtime_airplay;
        let kind = airplay_kind(r, prefer_realtime);
        self.settle_airplay_delay(r);
        // The stream it ends up with decides whether the change counts:
        // what is asked for is the same either way.
        let live = Live { buffered: Some(false), ..Default::default() };
        let s = self.delay_setting(&id, kind, &live);
        let bases = self.delay_bases();
        let homepod_short = matches!(kind, Kind::HomePodPair)
            || (matches!(kind, Kind::HomePod { .. }) && s.choice == Some(DelayChoice::Short));
        let homepod_ms = if homepod_short {
            s.buffer_ms()
        } else {
            // On Steady a HomePod's short delay still has its change, in
            // case the session falls back to the short profile.
            let stored = self.user_config.lock().unwrap().speaker_settings.get(&id).cloned().unwrap_or_default();
            setting(kind, &crate::user_config::SpeakerSettings { delay_choice: Some(DelayChoice::Short), ..stored }, &bases, &live)
                .buffer_ms()
        };
        self.delay.state().started = Some((id, s.change_ms));
        AirPlayDelay {
            latency_ms: if homepod_short { bases.steady_ms } else { s.buffer_ms() },
            homepod_realtime: homepod_short,
            homepod_latency_ms: homepod_ms,
        }
    }

    /// What a Sendspin session to `id` adds to the speaker's own delay,
    /// ms, noted as what it starts with.
    pub(crate) fn delay_for_sendspin(&self, id: &str) -> u32 {
        let change = self.delay_setting(id, Kind::Sendspin, &Live::default()).change_ms;
        self.delay.state().started = Some((id.to_string(), change));
        change.max(0) as u32
    }

    /// A session was bound (or a connect ended without one). A change made
    /// while it connected is applied: the stream's catch-up follows the
    /// UPnP speaker that plays, and another speaker reconnects if its
    /// session began with less or more than it now has.
    pub(crate) fn delay_session_bound(self: &Arc<Self>) {
        let Some(bound) = self.bound_for_delay() else { return };
        let s = self.delay_setting(&bound.id, bound.kind, &bound.live);
        let mut state = self.delay.state();
        if state.reconnect_for.as_deref().is_some_and(|r| r != bound.id) {
            state.schedule.cancel();
            state.reconnect_for = None;
        }
        match s.applies {
            Ok(Applies::Live) => {
                if state.upnp.as_deref() == Some(bound.id.as_str()) {
                    self.delay.shared.catch_up.set_target(s.change_ms);
                }
            }
            Ok(Applies::Reconnect) => {
                let started = state.started.as_ref().filter(|(sid, _)| *sid == bound.id).map(|(_, c)| *c);
                if started.is_some_and(|c| c != s.change_ms) && !state.schedule.waiting() {
                    state.reconnect_for = Some(bound.id.clone());
                    state.schedule.press(Instant::now());
                    drop(state);
                    self.delay_worker();
                }
            }
            Err(_) => {}
        }
    }

    /// The stream gave back some removed delay because the speaker ran
    /// dry: kept by the engine's thread.
    fn delay_backed_off(self: &Arc<Self>, b: BackOff) {
        self.delay.state().back_offs.push(b);
        self.delay_worker();
    }

    /// Have the stream's back-offs reach the engine.
    pub(crate) fn delay_watch_back_offs(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.delay.shared.catch_up.on_back_off(move |b| {
            if let Some(app) = weak.upgrade() {
                app.delay_backed_off(b);
            }
        });
    }

    // -------------------------------------------------------------------
    // The engine's thread
    // -------------------------------------------------------------------

    /// Wake the engine's thread, starting it the first time.
    fn delay_worker(self: &Arc<Self>) {
        self.delay.wake();
        if self.delay.shared.worker.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let shared = self.delay.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("stream-to-speaker-delay".into())
            .spawn(move || run_worker(weak, shared, &RealConnector));
        if spawned.is_err() {
            warn!("delay: couldn't start the thread that applies delay changes");
            self.delay.shared.worker.store(false, Ordering::Release);
        }
    }
}

/// How the engine's thread sees and connects the speaker that plays: the
/// app's sessions, or a test's stand-in.
trait Connector: Sync {
    /// The speaker that plays.
    fn bound(&self, app: &Arc<App>) -> Option<Bound>;
    /// Connect `id` again with what its settings hold now. `Err(None)`
    /// when another connect is under way; `Err(Some(why))` when it failed.
    fn connect_again(&self, app: &Arc<App>, id: &str) -> Result<(), Option<String>>;
}

struct RealConnector;

impl Connector for RealConnector {
    fn bound(&self, app: &Arc<App>) -> Option<Bound> {
        app.bound_for_delay()
    }

    fn connect_again(&self, app: &Arc<App>, id: &str) -> Result<(), Option<String>> {
        app.connect_again(id)
    }
}

/// How often the thread looks whether the app is quitting.
const IDLE_CHECK: Duration = Duration::from_millis(500);

fn run_worker(app: Weak<App>, shared: Arc<Shared>, connector: &dyn Connector) {
    loop {
        let Some(app) = app.upgrade() else { return };
        if app.is_shutting_down() {
            return;
        }
        let mut state = shared.state.lock().unwrap();
        // Back-offs the stream made: kept for the UPnP speaker that plays.
        let back_offs = std::mem::take(&mut state.back_offs);
        let upnp = state.upnp.clone();
        if let (Some(last), Some(id)) = (back_offs.last(), upnp.as_ref()) {
            let first = back_offs[0];
            state.notice = Some((id.clone(), Notice::DroppedOut { from_ms: first.from_ms, to_ms: last.to_ms }));
            state.changes += 1;
            drop(state);
            let mut uc = app.user_config.lock().unwrap();
            let mut entry = uc.speaker_settings.remove(id).unwrap_or_default();
            entry.delay_change_ms = last.to_ms;
            if !entry.is_default() {
                uc.speaker_settings.insert(id.clone(), entry);
            }
            drop(uc);
            state = shared.state.lock().unwrap();
        }
        if state.saved != state.changes {
            let changes = state.changes;
            drop(state);
            app.user_config.lock().unwrap().save();
            state = shared.state.lock().unwrap();
            state.saved = changes;
            shared.wake.notify_all();
        }
        let now = Instant::now();
        let due = state.schedule.due_at();
        if !due.is_some_and(|at| now >= at) {
            let wait = due.map_or(IDLE_CHECK, |at| at.saturating_duration_since(now).min(IDLE_CHECK));
            drop(app);
            let _ = shared.wake.wait_timeout(state, wait).unwrap();
            continue;
        }
        let Some(id) = state.reconnect_for.clone() else {
            state.schedule.cancel();
            continue;
        };
        state.schedule.begin(now);
        drop(state);
        reconnect_for_delay(&app, &shared, connector, &id);
        shared.state.lock().unwrap().schedule.finished();
        shared.wake.notify_all();
    }
}

/// Reconnect `id` to apply its delay change. If it doesn't accept the new
/// session, try once more with what it had; if that fails too, it is a
/// dropout.
fn reconnect_for_delay(app: &Arc<App>, shared: &Shared, connector: &dyn Connector, id: &str) {
    let Some(bound) = connector.bound(app).filter(|b| b.id == id) else {
        // It stopped playing meanwhile: the change applies when it next does.
        return;
    };
    if !app.is_streaming_enabled() {
        return;
    }
    let to_ms = app.delay_setting(id, bound.kind, &bound.live).change_ms;
    let from_ms = {
        let state = shared.state.lock().unwrap();
        state.started.as_ref().filter(|(sid, _)| sid == id).map_or(to_ms, |(_, c)| *c)
    };
    if to_ms == from_ms {
        return;
    }
    info!("delay: reconnecting {} to apply {:+} ms (had {:+} ms)", bound.name, to_ms, from_ms);
    shared.state.lock().unwrap().reconnecting = Some((id.to_string(), from_ms));
    let result = connector.connect_again(app, id);
    let failed = match result {
        Ok(()) => None,
        Err(None) => {
            // Another connect runs: try again once it is over.
            let mut state = shared.state.lock().unwrap();
            state.reconnecting = None;
            state.schedule.press(Instant::now());
            return;
        }
        Err(Some(why)) => Some(why),
    };
    let Some(why) = failed else {
        shared.state.lock().unwrap().reconnecting = None;
        return;
    };
    warn!("delay: {} didn't accept {:+} ms ({}); trying {:+} ms again", bound.name, to_ms, why, from_ms);
    // What it had goes back into the settings: the stored change returns.
    {
        let choice = app.delay_setting(id, bound.kind, &bound.live).choice;
        let mut uc = app.user_config.lock().unwrap();
        let mut entry = uc.speaker_settings.remove(id).unwrap_or_default();
        super::store_change(&mut entry, choice, from_ms);
        if !entry.is_default() {
            uc.speaker_settings.insert(id.to_string(), entry);
        }
    }
    let again_at = {
        let mut state = shared.state.lock().unwrap();
        state.changes += 1;
        state.notice = Some((id.to_string(), Notice::NotAccepted { tried_ms: to_ms, back_to_ms: from_ms }));
        state.schedule.again_at()
    };
    if let Some(at) = again_at {
        while Instant::now() < at {
            if app.is_shutting_down() {
                return;
            }
            std::thread::sleep(at.saturating_duration_since(Instant::now()).min(IDLE_CHECK));
        }
    }
    shared.state.lock().unwrap().schedule.began_again(Instant::now());
    let again = connector.connect_again(app, id);
    shared.state.lock().unwrap().reconnecting = None;
    if let Err(Some(why)) = again {
        let msg = format!("Couldn't connect to {}: {}", bound.name, why);
        app.record_fault(&msg);
        app.record_error(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::SpeakerSettings;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_speaker_that_doesnt_play_shows_its_stored_delay() {
        let app = App::for_tests();
        app.user_config
            .lock()
            .unwrap()
            .speaker_settings
            .insert("uuid:kitchen".into(), SpeakerSettings { delay_change_ms: -50, ..Default::default() });
        let s = app.delay_status_of("uuid:kitchen");
        assert_eq!((s.kind, s.delay_ms, s.own_delay_ms, s.change_ms), (Kind::Upnp, 150, 200, -50));
        assert_eq!(s.limits, Some(Limits { remove_ms: 200, add_ms: 1000 }));
        assert_eq!((s.applies, s.progress, s.notice), (Ok(Applies::Live), Progress::Done, None));
        assert_eq!(app.delay_status(), None, "nothing plays");
    }

    #[test]
    fn a_change_is_stored_at_once_and_stops_at_the_limit() {
        let app = App::for_tests();
        let out = app.change_delay(Some("uuid:kitchen"), Request::Remove(150)).unwrap();
        assert_eq!((out.from_ms, out.to_ms, out.clamped), (0, -150, false));
        let out = app.change_delay(Some("uuid:kitchen"), Request::Remove(100)).unwrap();
        assert_eq!((out.from_ms, out.to_ms, out.clamped), (-150, -200, true));
        assert_eq!(out.status.change_ms, -200);
        assert_eq!(app.user_config.lock().unwrap().speaker_settings["uuid:kitchen"].delay_change_ms, -200);
        // Back to the speaker's own delay: nothing is kept for it.
        app.change_delay(Some("uuid:kitchen"), Request::Set(0)).unwrap();
        assert!(!app.user_config.lock().unwrap().speaker_settings.contains_key("uuid:kitchen"));
        // No speaker plays: nothing to change.
        assert_eq!(app.change_delay(None, Request::Remove(25)), Err(Unchangeable::NoSpeaker));
        assert_eq!(
            app.change_delay(Some(crate::sendspin_app::MA_OUTPUT_ID), Request::Remove(25)),
            Err(Unchangeable::MusicAssistant)
        );
        app.request_shutdown();
    }

    #[test]
    fn each_choice_keeps_its_change_and_the_switch_says_so() {
        let app = App::for_tests();
        let id = "airplay:AA0000000001";
        assert_eq!(app.delay_status_of(id).choice, Some(DelayChoice::Steady));
        app.change_delay(Some(id), Request::Remove(300)).unwrap();
        let s = app.set_delay_choice(id, DelayChoice::Short).unwrap();
        assert_eq!((s.choice, s.change_ms, s.delay_ms), (Some(DelayChoice::Short), 0, 300));
        assert_eq!(s.notice, Some(Notice::ChoiceSwitched { to: DelayChoice::Short, now_ms: 0, other_ms: -300 }));
        app.change_delay(Some(id), Request::Remove(100)).unwrap();
        assert_eq!(app.delay_status_of(id).notice, None, "a change ends the notice");
        let s = app.set_delay_choice(id, DelayChoice::Steady).unwrap();
        assert_eq!((s.change_ms, s.delay_ms), (-300, 1700));
        // Speakers without a choice refuse one.
        assert!(app.set_delay_choice("uuid:kitchen", DelayChoice::Short).is_err());
        app.request_shutdown();
    }

    const OFFICE: &str = "airplay:0D0000000042";

    /// A speaker that plays, as far as the engine's thread can tell, whose
    /// connects the test scripts: each takes `hold`, and those numbered in
    /// `fails` aren't accepted.
    struct ScriptedSpeaker {
        hold: Duration,
        fails: Vec<usize>,
        /// Each connect: when it began, and what was removed or added then.
        connects: Mutex<Vec<(Instant, i32)>>,
        running: AtomicBool,
        overlapped: AtomicBool,
    }

    impl ScriptedSpeaker {
        fn new(hold: Duration, fails: &[usize]) -> Arc<Self> {
            Arc::new(Self {
                hold,
                fails: fails.to_vec(),
                connects: Mutex::new(Vec::new()),
                running: AtomicBool::new(false),
                overlapped: AtomicBool::new(false),
            })
        }

        fn connects(&self) -> Vec<(Instant, i32)> {
            self.connects.lock().unwrap().clone()
        }
    }

    impl Connector for ScriptedSpeaker {
        fn bound(&self, _: &Arc<App>) -> Option<Bound> {
            Some(Bound {
                id: OFFICE.into(),
                name: "Office".into(),
                kind: Kind::AirPlay { buffered_on_steady: false },
                live: Live::default(),
            })
        }

        fn connect_again(&self, app: &Arc<App>, id: &str) -> Result<(), Option<String>> {
            if self.running.swap(true, Ordering::AcqRel) {
                self.overlapped.store(true, Ordering::Release);
            }
            let change = app.user_config.lock().unwrap().speaker_settings.get(id).map_or(0, |s| s.steady_delay_change_ms);
            let n = {
                let mut c = self.connects.lock().unwrap();
                c.push((Instant::now(), change));
                c.len() - 1
            };
            // The connect itself: seconds on a real speaker.
            std::thread::sleep(self.hold);
            self.running.store(false, Ordering::Release);
            if self.fails.contains(&n) {
                return Err(Some("the speaker refused the session".into()));
            }
            // A session that came up began with the stored change.
            app.delay.state().started = Some((id.to_string(), change));
            Ok(())
        }
    }

    /// The app, with Office playing since a session that began with
    /// nothing removed, and the engine's thread running with `speaker`
    /// and short waits.
    fn with_office(speaker: &Arc<ScriptedSpeaker>) -> Arc<App> {
        let app = App::for_tests();
        {
            let mut state = app.delay.state();
            state.schedule = Schedule::with(Duration::from_millis(40), Duration::from_millis(150));
            state.started = Some((OFFICE.into(), 0));
        }
        let (weak, shared, speaker) = (Arc::downgrade(&app), app.delay.shared.clone(), speaker.clone());
        std::thread::spawn(move || run_worker(weak, shared, &*speaker));
        app
    }

    /// A press on Office while it plays (what `change_delay` does then).
    fn press(app: &App, change_ms: i32) {
        app.user_config.lock().unwrap().speaker_settings.entry(OFFICE.into()).or_default().steady_delay_change_ms = change_ms;
        let mut state = app.delay.state();
        state.changes += 1;
        state.notice = None;
        state.reconnect_for = Some(OFFICE.into());
        state.schedule.press(Instant::now());
        drop(state);
        app.delay.wake();
    }

    #[test]
    fn a_change_reconnects_once_the_presses_stop() {
        let speaker = ScriptedSpeaker::new(Duration::from_millis(20), &[]);
        let app = with_office(&speaker);
        let pressed = Instant::now();
        press(&app, -100);
        assert_eq!(app.delay_status_of(OFFICE).progress, Progress::WillReconnect);
        assert!(app.delay.wait_idle(Duration::from_secs(5)));
        let connects = speaker.connects();
        assert_eq!(connects.len(), 1);
        assert!(connects[0].0 >= pressed + Duration::from_millis(40));
        assert_eq!(connects[0].1, -100);
        assert_eq!(app.delay_status_of(OFFICE).progress, Progress::Done);
        app.request_shutdown();
    }

    #[test]
    fn presses_during_a_reconnect_make_one_more_after_it() {
        let speaker = ScriptedSpeaker::new(Duration::from_millis(100), &[]);
        let app = with_office(&speaker);
        press(&app, -100);
        // Once it runs, two more presses.
        while speaker.connects().is_empty() {
            app.delay.wait_idle(Duration::from_millis(10));
        }
        assert_eq!(app.delay_status_of(OFFICE).progress, Progress::Reconnecting { from_ms: 0 });
        press(&app, -125);
        press(&app, -150);
        assert!(app.delay.wait_idle(Duration::from_secs(5)));
        let connects = speaker.connects();
        assert_eq!(connects.iter().map(|c| c.1).collect::<Vec<_>>(), [-100, -150]);
        assert!(connects[1].0 >= connects[0].0 + Duration::from_millis(150), "too soon after the one before");
        assert!(!speaker.overlapped.load(Ordering::Acquire));
        app.request_shutdown();
    }

    #[test]
    fn a_value_the_speaker_refuses_goes_back_to_the_one_it_had() {
        let speaker = ScriptedSpeaker::new(Duration::from_millis(10), &[0]);
        let app = with_office(&speaker);
        press(&app, -200);
        assert!(app.delay.wait_idle(Duration::from_secs(5)));
        // Once more with what it had, after the usual spacing.
        let connects = speaker.connects();
        assert_eq!(connects.iter().map(|c| c.1).collect::<Vec<_>>(), [-200, 0]);
        assert!(connects[1].0 >= connects[0].0 + Duration::from_millis(150));
        let s = app.delay_status_of(OFFICE);
        assert_eq!(s.change_ms, 0);
        assert_eq!(s.notice, Some(Notice::NotAccepted { tried_ms: -200, back_to_ms: 0 }));
        // Not a dropout: nothing was said.
        assert_eq!(app.current_error(), None);
        // The next change ends the notice.
        press(&app, -25);
        assert_eq!(app.delay_status_of(OFFICE).notice, None);
        app.request_shutdown();
    }

    #[test]
    fn when_neither_value_is_accepted_it_is_a_dropout() {
        let speaker = ScriptedSpeaker::new(Duration::from_millis(10), &[0, 1]);
        let app = with_office(&speaker);
        press(&app, -200);
        assert!(app.delay.wait_idle(Duration::from_secs(5)));
        assert_eq!(speaker.connects().len(), 2, "no third try");
        let said = app.current_error().unwrap_or_default();
        assert!(said.starts_with("Couldn't connect to Office"), "{said}");
        app.request_shutdown();
    }

    #[test]
    fn a_change_back_before_the_reconnect_needs_none() {
        let speaker = ScriptedSpeaker::new(Duration::from_millis(10), &[]);
        let app = with_office(&speaker);
        press(&app, -25);
        press(&app, 0);
        assert!(app.delay.wait_idle(Duration::from_secs(5)));
        assert!(speaker.connects().is_empty());
        app.request_shutdown();
    }

    #[test]
    fn the_engine_thread_saves_what_changed() {
        let dir = std::env::temp_dir().join(format!("sts-delay-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let app = App::with_settings(
            App::test_config(),
            crate::user_config::UserConfig::at_path(path.clone()),
            None,
            None,
        );
        app.change_delay(Some("uuid:kitchen"), Request::Add(100)).unwrap();
        assert!(app.delay.wait_idle(Duration::from_secs(5)), "the change wasn't saved");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        app.request_shutdown();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(text.contains("\"delay_change_ms\": 100"), "{text}");
    }
}
