//! Bidirectional volume sync between the Windows endpoint slider and the
//! speaker, with echo suppression so a change made on one side doesn't
//! bounce back from the other.
//!
//! Both sides speak the same linear `0..=100` scale:
//!
//!  * Windows: our driver exposes a hardware volume node advertising a
//!    −96..0 dB range (`KSPROPERTY_AUDIO_VOLUMELEVEL`). For a hardware
//!    volume node Windows maps the slider **linearly across that dB
//!    range** — slider 50 % lands on −48 dB, the midpoint, *not* on the
//!    perceptual ("audio-tapered") curve that `IAudioEndpointVolume`'s
//!    scalar methods use. So we receive the dB over the IOCTL and convert
//!    it back to a percent **linearly**; that recovers the slider
//!    position 1:1.
//!  * Speaker (Sonos/UPnP): the `RenderingControl` `Volume` is already a
//!    `0..=100` value matching the speaker's own UI slider.
//!
//! Earlier this used a logarithmic `100 * 10^(mb/5000)` conversion, on
//! the assumption that Windows applied a perceptual taper to the dB. It
//! doesn't (for a hardware node), so that double-counted the curve and
//! made e.g. a 50 % slider show as ~11 % on the speaker. The mapping is
//! plain linear.

use log::debug;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the speaker's reports of levels we sent are taken for echoes,
/// counted from the last level queued or the last send to the speaker
/// starting or finishing. UPnP RenderingControl events are moderated (one
/// LastChange per 0.2 s at most, per the service spec), so a report can
/// trail the change by that much plus delivery, and during a drag it
/// names a level from earlier in the drag rather than the latest.
const SPEAKER_ECHO_WINDOW: Duration = Duration::from_secs(1);

/// Same for levels we set on the Windows slider, which the driver
/// reports back at once.
const WINDOWS_ECHO_WINDOW: Duration = Duration::from_millis(250);

/// Bottom of the dB range our driver's volume node advertises, in
/// millibels. MUST match `STREAM_TO_SPEAKER_VOLUME_MIN_MILLIBELS` in
/// `driver/driver.h` (−9600 = −96 dB) — Windows maps the slider linearly
/// across `[VOLUME_MIN_MILLIBELS, 0]`, so this is the 0 % anchor.
const VOLUME_MIN_MILLIBELS: i32 = -9600;

/// Called after the level changes; see [`VolumeSync::set_on_change`].
type ChangeHook = Arc<dyn Fn() + Send + Sync>;

pub struct VolumeSync {
    inner: Mutex<Inner>,
    /// Last known mute state of the shared endpoint, from either side.
    muted: AtomicBool,
    on_change: Mutex<Option<ChangeHook>>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The levels being sent to the speaker, so its reports of them are
    /// not taken for changes made on the speaker.
    to_speaker: SpeakerRun,
    /// Levels recently pushed to the Windows slider, so the driver-event
    /// echoes of every one of them are dropped, not only the latest.
    sent_to_windows: SentLevels,
    /// Most recent committed level, for dedup and for the GUI slider.
    last_level: Option<u32>,
    /// A level was queued for the speaker since it was bound.
    level_set_since_bound: bool,
}

/// Levels pushed to the Windows slider in the current run of pushes,
/// oldest first, each once (so never more than 101). A run lasts while
/// pushes keep coming within the echo window of each other.
#[derive(Debug, Default)]
struct SentLevels {
    levels: Vec<u32>,
    last_push: Option<Instant>,
}

impl SentLevels {
    fn live(&self, window: Duration, now: Instant) -> bool {
        self.last_push.is_some_and(|t| now.saturating_duration_since(t) < window)
    }

    /// `level` was pushed. Starts a new run if the last one is over.
    fn remember(&mut self, level: u32, window: Duration, now: Instant) {
        if !self.live(window, now) {
            self.levels.clear();
        }
        self.levels.retain(|&l| l != level);
        self.levels.push(level);
        self.last_push = Some(now);
    }

    /// Was `level` pushed in a run that is still going?
    fn sent_within(&self, level: u32, window: Duration, now: Instant) -> bool {
        self.live(window, now) && self.levels.contains(&level)
    }
}

/// The current run of volume sends to the speaker. A run lasts while a
/// send is under way, and for [`SPEAKER_ECHO_WINDOW`] after a level was
/// last queued or a send last started or finished — behind a slow
/// speaker that is well after the drag that queued them. While it lasts,
/// a report naming a level that was sent in it is the speaker catching
/// up, not a change made on the speaker. The latest such report is kept:
/// when the run ends it tells where the speaker ended up, which is not
/// always the newest level (a speaker that caps its volume, or a change
/// on the speaker that happens to land on a level of the drag).
#[derive(Debug, Default)]
struct SpeakerRun {
    /// Levels whose send started in this run, each once. Levels that
    /// were queued but replaced by a newer one before going out are not
    /// here: the speaker never heard of them.
    sent: Vec<u32>,
    in_flight: bool,
    last_activity: Option<Instant>,
    /// The speaker's latest report that named a level of this run.
    last_echo: Option<u32>,
}

impl SpeakerRun {
    fn live(&self, now: Instant) -> bool {
        self.in_flight || self.last_activity.is_some_and(|t| now.saturating_duration_since(t) < SPEAKER_ECHO_WINDOW)
    }

    /// How long until the run ends, if it is still going.
    fn ends_in(&self, now: Instant) -> Option<Duration> {
        if self.in_flight {
            return Some(SPEAKER_ECHO_WINDOW);
        }
        let since = now.saturating_duration_since(self.last_activity?);
        SPEAKER_ECHO_WINDOW.checked_sub(since).filter(|left| !left.is_zero())
    }

    /// A level was queued: keep the run going, or start a new one.
    fn touch(&mut self, now: Instant) {
        if !self.live(now) {
            *self = SpeakerRun::default();
        }
        self.last_activity = Some(now);
    }

    fn start(&mut self, level: u32, now: Instant) {
        self.touch(now);
        if !self.sent.contains(&level) {
            self.sent.push(level);
        }
        self.in_flight = true;
    }

    /// The send under way finished. Ignored when the run was cleared
    /// meanwhile: that send went to a speaker that is no longer bound.
    fn done(&mut self, now: Instant) {
        if self.in_flight {
            self.in_flight = false;
            self.last_activity = Some(now);
        }
    }
}

impl VolumeSync {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            muted: AtomicBool::new(false),
            on_change: Mutex::new(None),
        }
    }

    /// Run `hook` whenever the level changes, from whichever side. The
    /// window uses it to repaint at once: the level arrives on other
    /// threads, and waiting for the next heartbeat or input would leave
    /// the volume row behind the Windows slider. Called without any lock
    /// held.
    pub fn set_on_change(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_change.lock().unwrap() = Some(Arc::new(hook));
    }

    fn notify_change(&self) {
        let hook = self.on_change.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// The Windows slider moved (reported via the driver event). `level`
    /// is `0..=100`. Returns the level to queue for the speaker, or
    /// `None` if it's an echo / no change.
    pub fn driver_changed(&self, level: u32) -> Option<u32> {
        self.driver_changed_at(level, Instant::now())
    }

    fn driver_changed_at(&self, level: u32, now: Instant) -> Option<u32> {
        let level = level.min(100);
        {
            let mut inner = self.inner.lock().unwrap();
            // If we *just* set this on the Windows slider (from a speaker
            // change), this is the echo — drop it.
            if inner.sent_to_windows.sent_within(level, WINDOWS_ECHO_WINDOW, now) {
                debug!("ignoring windows echo: level={}", level);
                return None;
            }
            if Some(level) == inner.last_level {
                return None;
            }
            inner.to_speaker.touch(now);
            inner.level_set_since_bound = true;
            inner.last_level = Some(level);
        }
        self.notify_change();
        Some(level)
    }

    /// The speaker reported a new volume (`0..=100`, via GENA NOTIFY).
    /// Returns the level to set on the Windows slider, or `None` if it's
    /// an echo / no change.
    pub fn sonos_changed(&self, level: u32) -> Option<u32> {
        self.sonos_changed_at(level, Instant::now())
    }

    fn sonos_changed_at(&self, level: u32, now: Instant) -> Option<u32> {
        let level = level.min(100);
        {
            let mut inner = self.inner.lock().unwrap();
            let run = &mut inner.to_speaker;
            if run.live(now) {
                // A level sent in this run, even one we have since moved
                // past: most likely the speaker catching up. Where it ends
                // up is settled when the run ends.
                if run.sent.contains(&level) {
                    debug!("ignoring speaker echo: level={}", level);
                    run.last_echo = Some(level);
                    return None;
                }
                // A level we never sent: the speaker's own doing.
                run.last_echo = None;
            } else {
                // Newer than anything the run kept.
                *run = SpeakerRun::default();
            }
            if Some(level) == inner.last_level {
                return None;
            }
            inner.sent_to_windows.remember(level, WINDOWS_ECHO_WINDOW, now);
            inner.last_level = Some(level);
        }
        self.notify_change();
        Some(level)
    }

    /// The window's own volume slider set `level` (`0..=100`), which is
    /// queued for the speaker. Returns whether the level changed, in which
    /// case the caller sets the Windows slider to it too. Its echoes from
    /// both sides are dropped like those of the other paths.
    pub fn window_changed(&self, level: u32) -> bool {
        self.window_changed_at(level, Instant::now())
    }

    fn window_changed_at(&self, level: u32, now: Instant) -> bool {
        let level = level.min(100);
        {
            let mut inner = self.inner.lock().unwrap();
            inner.to_speaker.touch(now);
            inner.level_set_since_bound = true;
            if Some(level) == inner.last_level {
                return false;
            }
            inner.sent_to_windows.remember(level, WINDOWS_ECHO_WINDOW, now);
            inner.last_level = Some(level);
        }
        self.notify_change();
        true
    }

    /// The volume thread is sending `level` to the speaker; reports of it
    /// can come from now on.
    pub fn speaker_send_start(&self, level: u32) {
        self.speaker_send_start_at(level, Instant::now());
    }

    fn speaker_send_start_at(&self, level: u32, now: Instant) {
        self.inner.lock().unwrap().to_speaker.start(level.min(100), now);
    }

    /// The send started last finished, successfully or not. The speaker's
    /// report of it comes around then, which behind a slow speaker is well
    /// after it was queued.
    pub fn speaker_send_done(&self) {
        self.speaker_send_done_at(Instant::now());
    }

    fn speaker_send_done_at(&self, now: Instant) {
        self.inner.lock().unwrap().to_speaker.done(now);
    }

    /// How long until the current run of sends to the speaker ends;
    /// `None` once it has, when [`Self::speaker_settle`] is due.
    pub fn speaker_run_ends_in(&self) -> Option<Duration> {
        self.inner.lock().unwrap().to_speaker.ends_in(Instant::now())
    }

    /// The run of sends to the speaker is over: if the speaker's last
    /// report (a UPnP event; no other protocol's reports reach this)
    /// named a level other than ours, that is where it ended up.
    /// Returns that level, now the current one, to set on the Windows
    /// slider; `None` when the speaker agrees or nothing is known.
    pub fn speaker_settle(&self) -> Option<u32> {
        self.speaker_settle_at(Instant::now())
    }

    fn speaker_settle_at(&self, now: Instant) -> Option<u32> {
        let level = {
            let mut inner = self.inner.lock().unwrap();
            if inner.to_speaker.live(now) {
                return None;
            }
            let reported = std::mem::take(&mut inner.to_speaker).last_echo?;
            if Some(reported) == inner.last_level {
                return None;
            }
            inner.sent_to_windows.remember(reported, WINDOWS_ECHO_WINDOW, now);
            inner.last_level = Some(reported);
            reported
        };
        debug!("speaker ended its run of changes at {}", level);
        self.notify_change();
        Some(level)
    }

    /// Another speaker was bound: nothing sent to the old one is an echo
    /// from this one.
    pub fn speaker_bound(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.to_speaker = SpeakerRun::default();
        inner.level_set_since_bound = false;
    }

    /// The level a newly bound speaker is at, read or set as its session
    /// started. It becomes the current level unless a level was already
    /// queued for this speaker since it was bound, which then wins.
    /// Returns the level to set on the Windows slider, like
    /// [`Self::sonos_changed`].
    pub fn speaker_connected(&self, level: u32) -> Option<u32> {
        if self.inner.lock().unwrap().level_set_since_bound {
            return None;
        }
        self.sonos_changed(level)
    }
}

impl VolumeSync {
    /// Last-known volume level (`0..=100`), from whichever side set it
    /// last. The GUI reads this to render the volume slider (m24).
    pub fn current_level(&self) -> Option<u32> {
        self.inner.lock().unwrap().last_level
    }

    /// Record a mute change seen on either side (Windows or speaker).
    pub fn note_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    /// Apply a speaker event (GENA) to our state and work out what to
    /// hand the driver. The driver's PUSH_VOLUME sets level *and* mute
    /// together, so every push carries both: a mute event keeps the
    /// current level and a volume event keeps the current mute state.
    /// `volume` is the speaker's raw `0..=100`. Returns `(millibels,
    /// muted)`, or `None` when nothing needs pushing (echo, no change,
    /// or a mute change while no level is known yet).
    pub fn speaker_event(&self, volume: Option<u32>, mute: Option<bool>) -> Option<(i32, bool)> {
        let new_level = volume.and_then(|v| self.sonos_changed(v));
        let mute_changed = match mute {
            Some(m) => self.muted.swap(m, Ordering::AcqRel) != m,
            None => false,
        };
        if new_level.is_none() && !mute_changed {
            return None;
        }
        let level = new_level.or_else(|| self.current_level())?;
        Some((sonos_to_millibels(level), self.is_muted()))
    }
}

impl Default for VolumeSync {
    fn default() -> Self {
        Self::new()
    }
}

/// A change for the bound speaker's volume control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerCommand {
    Volume(u32),
    Mute(bool),
}

/// Most changes kept waiting; past this the oldest go. With runs of one
/// kind collapsed, only alternating volume and mute changes can fill it,
/// and the newest of each kind are always kept.
const MAX_PENDING_COMMANDS: usize = 16;

/// Changes waiting to be sent to the speaker, oldest first, each with
/// the speaker session it was meant for. A change replaces a waiting
/// change of the same kind at the back, so while one send is in flight a
/// slider drag collapses into its newest level — the speaker gets that
/// next instead of working through every level in between — and a mute
/// stays ordered against the levels around it (RAOP mutes by volume, so
/// the order decides what the speaker ends on).
#[derive(Debug, Default)]
pub struct PendingCommands {
    queue: VecDeque<(u64, SpeakerCommand)>,
}

impl PendingCommands {
    /// Queue `cmd` for the speaker session numbered `session`.
    pub fn push(&mut self, session: u64, cmd: SpeakerCommand) {
        match (self.queue.back_mut(), cmd) {
            (Some((s, SpeakerCommand::Volume(v))), SpeakerCommand::Volume(n)) => (*s, *v) = (session, n),
            (Some((s, SpeakerCommand::Mute(m))), SpeakerCommand::Mute(n)) => (*s, *m) = (session, n),
            _ => {
                if self.queue.len() >= MAX_PENDING_COMMANDS {
                    self.queue.pop_front();
                }
                self.queue.push_back((session, cmd));
            }
        }
    }

    pub fn pop(&mut self) -> Option<(u64, SpeakerCommand)> {
        self.queue.pop_front()
    }
}

/// Convert a Windows millibel value from our volume node to a `0..=100`
/// level. Linear across `[VOLUME_MIN_MILLIBELS, 0]` — see the module
/// docs for why this isn't logarithmic.
pub fn millibels_to_sonos(mb: i32) -> u32 {
    if mb >= 0 {
        return 100;
    }
    if mb <= VOLUME_MIN_MILLIBELS {
        return 0;
    }
    let frac = (mb - VOLUME_MIN_MILLIBELS) as f64 / (-VOLUME_MIN_MILLIBELS) as f64;
    (frac * 100.0).round().clamp(0.0, 100.0) as u32
}

/// Inverse of `millibels_to_sonos`: a `0..=100` level to the millibel
/// value to set on our node so the Windows slider shows that percent.
pub fn sonos_to_millibels(level: u32) -> i32 {
    let level = level.min(100);
    if level == 0 {
        return VOLUME_MIN_MILLIBELS;
    }
    if level >= 100 {
        return 0;
    }
    let mb = VOLUME_MIN_MILLIBELS as f64 * (1.0 - level as f64 / 100.0);
    mb.round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millibels_roundtrip() {
        for level in [0, 1, 10, 25, 50, 75, 100] {
            let mb = sonos_to_millibels(level);
            let back = millibels_to_sonos(mb);
            assert!(
                (back as i32 - level as i32).abs() <= 1,
                "level {} -> {} -> {}",
                level,
                mb,
                back
            );
        }
        assert_eq!(millibels_to_sonos(0), 100);
        assert_eq!(millibels_to_sonos(VOLUME_MIN_MILLIBELS), 0);
        assert_eq!(millibels_to_sonos(-10000), 0); // clamps below the range
    }

    #[test]
    fn conversion_is_linear() {
        // The whole point of the fix: slider % maps 1:1 through the dB.
        // These are the midpoints Windows produces for our −9600..0 node.
        assert_eq!(millibels_to_sonos(-4800), 50);
        assert_eq!(millibels_to_sonos(-2400), 75);
        assert_eq!(millibels_to_sonos(-7680), 20);
        assert_eq!(sonos_to_millibels(50), -4800);
        assert_eq!(sonos_to_millibels(25), -7200);
    }

    #[test]
    fn speaker_echo_is_ignored() {
        let s = VolumeSync::new();
        // Speaker tells us volume=50; we forward it to the Windows slider.
        assert_eq!(s.sonos_changed(50), Some(50));
        // The driver immediately echoes the new level back. Dropped.
        assert_eq!(s.driver_changed(50), None);
    }

    #[test]
    fn windows_echo_is_ignored() {
        let s = VolumeSync::new();
        // Windows slider moves to 30; we forward it to the speaker.
        assert_eq!(s.driver_changed(30), Some(30));
        // The speaker's GENA NOTIFY echoes 30 back. Dropped.
        assert_eq!(s.sonos_changed(30), None);
    }

    #[test]
    fn distinct_changes_pass_through() {
        let s = VolumeSync::new();
        assert_eq!(s.driver_changed(40), Some(40));
        // A genuine, different speaker-side change is forwarded.
        assert_eq!(s.sonos_changed(70), Some(70));
        // Re-sending the same level is a no-op.
        assert_eq!(s.sonos_changed(70), None);
    }

    #[test]
    fn speaker_mute_keeps_the_level() {
        let v = VolumeSync::new();
        assert_eq!(v.speaker_connected(40), Some(40));
        // Mute alone: same level, muted — never 0 dB (= 100 %).
        assert_eq!(v.speaker_event(None, Some(true)), Some((sonos_to_millibels(40), true)));
        // Repeated mute state: nothing to push.
        assert_eq!(v.speaker_event(None, Some(true)), None);
        // A volume change while muted stays muted.
        assert_eq!(v.speaker_event(Some(55), None), Some((sonos_to_millibels(55), true)));
        assert_eq!(v.speaker_event(Some(55), Some(false)), Some((sonos_to_millibels(55), false)));
    }

    #[test]
    fn speaker_mute_without_known_level_is_not_pushed() {
        let v = VolumeSync::new();
        assert_eq!(v.speaker_event(None, Some(true)), None);
        assert!(v.is_muted());
        assert_eq!(v.speaker_event(Some(30), None), Some((sonos_to_millibels(30), true)));
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A Windows drag through `levels`, one every 10 ms from `t0`, each
    /// sent to a speaker that takes 5 ms per send. Returns when the last
    /// send finished.
    fn drag(s: &VolumeSync, levels: impl IntoIterator<Item = u32>, t0: Instant) -> Instant {
        let mut t = t0;
        let mut done = t0;
        for level in levels {
            assert_eq!(s.driver_changed_at(level, t), Some(level));
            s.speaker_send_start_at(level, t);
            done = t + ms(5);
            s.speaker_send_done_at(done);
            t += ms(10);
        }
        done
    }

    #[test]
    fn late_speaker_reports_during_a_drag_are_echoes() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        let done = drag(&s, 30..=50, t0);
        // The speaker's moderated events name levels from earlier in the
        // drag; none of them is a change made on the speaker.
        assert_eq!(s.sonos_changed_at(38, t0 + ms(210)), None);
        assert_eq!(s.sonos_changed_at(46, t0 + ms(400)), None);
        // The final level's report lands well after it was sent.
        assert_eq!(s.sonos_changed_at(50, t0 + ms(600)), None);
        assert_eq!(s.current_level(), Some(50));
        // The speaker ended where we did: nothing to correct.
        assert_eq!(s.speaker_settle_at(done + SPEAKER_ECHO_WINDOW), None);
        assert_eq!(s.current_level(), Some(50));
    }

    #[test]
    fn levels_never_sent_are_the_speakers_own() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        for (i, level) in (30..=50u32).enumerate() {
            s.driver_changed_at(level, t0 + ms(10 * i as u64));
        }
        // Behind a slower speaker the drag went out as 30, 41 and 50.
        for (i, level) in [30, 41, 50].into_iter().enumerate() {
            s.speaker_send_start_at(level, t0 + ms(100 * i as u64));
            s.speaker_send_done_at(t0 + ms(100 * i as u64 + 100));
        }
        // 45 was passed over in the drag but never sent: the speaker
        // capped its volume there.
        assert_eq!(s.sonos_changed_at(45, t0 + ms(350)), Some(45));
        assert_eq!(s.current_level(), Some(45));
        // A drag level reported long after the drag: a real change too.
        assert_eq!(s.sonos_changed_at(41, t0 + ms(2_000)), Some(41));
        assert_eq!(s.current_level(), Some(41));
    }

    #[test]
    fn a_speaker_stopping_short_of_the_drag_is_believed_once_the_run_ends() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        // The speaker caps at 40, a level the drag did send.
        let done = drag(&s, 30..=60, t0);
        assert_eq!(s.sonos_changed_at(35, t0 + ms(200)), None);
        assert_eq!(s.sonos_changed_at(40, t0 + ms(400)), None);
        // While the run lasts a later report could still name 60.
        assert_eq!(s.speaker_settle_at(done + ms(500)), None);
        assert_eq!(s.current_level(), Some(60));
        assert_eq!(s.speaker_settle_at(done + SPEAKER_ECHO_WINDOW), Some(40));
        assert_eq!(s.current_level(), Some(40));
        // Windows reports the level pushed to its slider straight back.
        assert_eq!(s.driver_changed_at(40, done + SPEAKER_ECHO_WINDOW + ms(10)), None);
        // Settled once.
        assert_eq!(s.speaker_settle_at(done + ms(3_000)), None);
    }

    #[test]
    fn a_change_on_the_speaker_landing_on_a_drag_level_is_kept() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        let done = drag(&s, [52, 50, 48], t0);
        assert_eq!(s.sonos_changed_at(48, t0 + ms(300)), None);
        // + pressed on the speaker, landing on a level sent moments ago.
        assert_eq!(s.sonos_changed_at(50, t0 + ms(600)), None);
        assert_eq!(s.speaker_settle_at(done + SPEAKER_ECHO_WINDOW), Some(50));
        assert_eq!(s.current_level(), Some(50));
    }

    #[test]
    fn a_level_sent_twice_in_a_drag_does_not_bounce() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        // 40 → 50 → 40, behind a speaker taking 300 ms per send.
        assert_eq!(s.driver_changed_at(40, t0), Some(40));
        s.speaker_send_start_at(40, t0);
        assert_eq!(s.driver_changed_at(50, t0 + ms(100)), Some(50));
        assert_eq!(s.driver_changed_at(40, t0 + ms(200)), Some(40));
        s.speaker_send_done_at(t0 + ms(300));
        s.speaker_send_start_at(50, t0 + ms(300));
        s.speaker_send_done_at(t0 + ms(600));
        s.speaker_send_start_at(40, t0 + ms(600));
        s.speaker_send_done_at(t0 + ms(900));
        // Its reports trail the sends: the first 40, then 50, then 40.
        assert_eq!(s.sonos_changed_at(40, t0 + ms(950)), None);
        assert_eq!(s.sonos_changed_at(50, t0 + ms(1_150)), None);
        assert_eq!(s.current_level(), Some(40));
        assert_eq!(s.sonos_changed_at(40, t0 + ms(1_350)), None);
        assert_eq!(s.speaker_settle_at(t0 + ms(2_000)), None);
        assert_eq!(s.current_level(), Some(40));
    }

    #[test]
    fn a_slow_speaker_reports_echoes_after_the_send_not_the_drag() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        // Windows slider dragged 30 → 50; the speaker takes 800 ms per
        // SetVolume, so 50 goes out only once 30 is done.
        for (i, level) in (30..=50u32).enumerate() {
            s.driver_changed_at(level, t0 + ms(10 * i as u64));
        }
        s.speaker_send_start_at(30, t0);
        s.speaker_send_done_at(t0 + ms(800));
        s.speaker_send_start_at(50, t0 + ms(800));
        assert_eq!(s.sonos_changed_at(30, t0 + ms(1_100)), None);
        s.speaker_send_done_at(t0 + ms(1_600));
        assert_eq!(s.sonos_changed_at(50, t0 + ms(1_900)), None);
        assert_eq!(s.current_level(), Some(50));
        assert_eq!(s.speaker_settle_at(t0 + ms(2_600)), None);
        // After the run a drag level is a change on the speaker.
        assert_eq!(s.sonos_changed_at(30, t0 + ms(2_700)), Some(30));
    }

    #[test]
    fn a_send_that_hangs_keeps_the_run_until_it_ends() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        drag(&s, 40..=60, t0);
        // The next level's send waits out a connect timeout.
        assert_eq!(s.driver_changed_at(61, t0 + ms(300)), Some(61));
        s.speaker_send_start_at(61, t0 + ms(300));
        assert_eq!(s.speaker_settle_at(t0 + ms(4_000)), None);
        s.speaker_send_done_at(t0 + ms(5_300));
        // A drag level reported meanwhile could be the speaker catching
        // up; once the run is over it is where the speaker is.
        assert_eq!(s.sonos_changed_at(45, t0 + ms(5_500)), None);
        assert_eq!(s.speaker_settle_at(t0 + ms(6_400)), Some(45));
    }

    #[test]
    fn a_new_speaker_owes_nothing_to_the_old_ones_run() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        drag(&s, 40..=50, t0);
        assert_eq!(s.driver_changed_at(70, t0 + ms(200)), Some(70));
        s.speaker_send_start_at(70, t0 + ms(200));
        s.speaker_bound();
        // The old speaker's send ending after the switch revives nothing.
        s.speaker_send_done_at(t0 + ms(400));
        assert!(!s.inner.lock().unwrap().to_speaker.live(t0 + ms(410)));
        // The new speaker's first report names a level the old one was
        // sent: it is the new speaker's level.
        assert_eq!(s.sonos_changed_at(45, t0 + ms(420)), Some(45));
    }

    #[test]
    fn the_level_read_on_connect_yields_to_one_set_since() {
        let s = VolumeSync::new();
        assert_eq!(s.driver_changed(50), Some(50));
        s.speaker_bound();
        // Nothing set since the speaker was bound: its level wins.
        assert_eq!(s.speaker_connected(20), Some(20));
        s.speaker_bound();
        assert_eq!(s.driver_changed(55), Some(55));
        // Read before the newer level went out.
        assert_eq!(s.speaker_connected(20), None);
        assert_eq!(s.current_level(), Some(55));
    }

    #[test]
    fn window_slider_levels_are_echoed_like_the_others() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        assert!(s.window_changed_at(30, t0));
        s.speaker_send_start_at(30, t0);
        s.speaker_send_done_at(t0 + ms(20));
        assert!(s.window_changed_at(60, t0 + ms(100)));
        s.speaker_send_start_at(60, t0 + ms(100));
        s.speaker_send_done_at(t0 + ms(120));
        assert_eq!(s.current_level(), Some(60));
        // Windows reports the level pushed to its slider straight back.
        assert_eq!(s.driver_changed_at(60, t0 + ms(110)), None);
        // The speaker's late report of the first click is an echo.
        assert_eq!(s.sonos_changed_at(30, t0 + ms(400)), None);
        assert_eq!(s.sonos_changed_at(60, t0 + ms(600)), None);
        assert_eq!(s.current_level(), Some(60));
        // The same level again: still sent, nothing to push to Windows.
        assert!(!s.window_changed_at(60, t0 + ms(700)));
        s.speaker_send_start_at(60, t0 + ms(700));
        s.speaker_send_done_at(t0 + ms(720));
        assert_eq!(s.sonos_changed_at(60, t0 + ms(900)), None);
        assert_eq!(s.speaker_settle_at(t0 + ms(1_800)), None);
    }

    #[test]
    fn driver_echoes_of_a_speaker_drag_are_dropped() {
        let s = VolumeSync::new();
        let t0 = Instant::now();
        for (i, level) in (60..=70u32).enumerate() {
            assert_eq!(s.sonos_changed_at(level, t0 + ms(20 * i as u64)), Some(level));
        }
        // Windows reports an earlier level of that drag after the last one.
        assert_eq!(s.driver_changed_at(64, t0 + ms(210)), None);
        assert_eq!(s.current_level(), Some(70));
        // Past the window the same level is the user moving the slider.
        assert_eq!(s.driver_changed_at(64, t0 + ms(800)), Some(64));
    }

    #[test]
    fn level_changes_wake_the_window() {
        let s = VolumeSync::new();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = wakes.clone();
        s.set_on_change(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        let count = || wakes.load(Ordering::SeqCst);
        assert_eq!(s.driver_changed(20), Some(20));
        assert_eq!(count(), 1);
        // Each step of a slider drag is a change the window must show.
        assert_eq!(s.driver_changed(21), Some(21));
        assert_eq!(count(), 2);
        // Repeats and echoes change nothing.
        assert_eq!(s.driver_changed(21), None);
        assert_eq!(s.sonos_changed(21), None);
        assert_eq!(count(), 2);
        assert_eq!(s.sonos_changed(60), Some(60));
        assert_eq!(count(), 3);
        s.speaker_bound();
        assert_eq!(s.speaker_connected(60), None);
        assert_eq!(count(), 3);
        assert_eq!(s.speaker_connected(35), Some(35));
        assert_eq!(count(), 4);
        assert_eq!(s.current_level(), Some(35));
    }

    fn drain(p: &mut PendingCommands) -> Vec<SpeakerCommand> {
        std::iter::from_fn(|| p.pop().map(|(_, cmd)| cmd)).collect()
    }

    #[test]
    fn a_drag_collapses_into_its_newest_level() {
        use SpeakerCommand::*;
        let mut p = PendingCommands::default();
        for level in 30..=50 {
            p.push(1, Volume(level));
        }
        assert_eq!(drain(&mut p), vec![Volume(50)]);
        assert_eq!(p.pop(), None);
    }

    #[test]
    fn a_collapsed_change_is_for_the_newer_session() {
        use SpeakerCommand::*;
        let mut p = PendingCommands::default();
        p.push(1, Volume(30));
        p.push(2, Volume(40));
        assert_eq!(p.pop(), Some((2, Volume(40))));
    }

    #[test]
    fn mute_keeps_its_place_among_levels() {
        use SpeakerCommand::*;
        let mut p = PendingCommands::default();
        p.push(1, Volume(30));
        p.push(1, Volume(35));
        p.push(1, Mute(true));
        p.push(1, Volume(40));
        p.push(1, Volume(45));
        assert_eq!(drain(&mut p), vec![Volume(35), Mute(true), Volume(45)]);
        p.push(1, Mute(true));
        p.push(1, Mute(false));
        assert_eq!(drain(&mut p), vec![Mute(false)]);
    }

    #[test]
    fn pending_commands_stay_bounded_and_keep_the_newest() {
        use SpeakerCommand::*;
        let mut p = PendingCommands::default();
        for i in 0..100u32 {
            p.push(1, Volume(i));
            p.push(1, Mute(i % 2 == 0));
        }
        let got = drain(&mut p);
        assert_eq!(got.len(), MAX_PENDING_COMMANDS);
        assert_eq!(&got[got.len() - 2..], &[Volume(99), Mute(false)]);
    }

    #[test]
    fn level_is_clamped() {
        let s = VolumeSync::new();
        assert_eq!(s.driver_changed(150), Some(100));
    }
}
