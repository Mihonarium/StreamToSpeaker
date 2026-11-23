//! Audio loop: takes packets from the [`AudioSource`], processes them and
//! publishes them to the stream hub.
//!
//! Two threads. The reader owns the source and does nothing but wait in
//! `recv_packet`, handing each packet over a channel. That wait lasts as
//! long as Windows plays nothing: the driver holds a read until a stream
//! runs, and WASAPI loopback delivers nothing in silence. The loop thread
//! processes what arrives and, once the source has been quiet for
//! [`QUIET_BEFORE_SILENCE`] (or at once when the driver says its stream
//! stopped), publishes paced silence in its place, so a speaker pulling the
//! HTTP stream keeps getting data: the server ends a stream after 30 s
//! without any. The next packet from the source ends the silence at once.
//!
//! When the source fails (the sound device was disabled, removed or
//! updated under the app), the reader opens it again after a growing
//! delay; the silence goes on meanwhile and the window says what happened.

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use log::{debug, info, warn};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::audio_source::{AudioPacket, AudioSource, PACKET_FLAG_HINT_SILENT, PACKET_FLAG_STREAM_RESTART};
use crate::equalizer::Equalizer;
use crate::http_server::{samples_to_wav_le_bytes, PcmFrame};
use crate::silence::SilenceDetector;
use crate::{WIRE_CHANNELS, WIRE_SAMPLE_RATE};

/// Opens the source again after it failed.
pub type Reopen = Box<dyn FnMut() -> Result<Box<dyn AudioSource>> + Send>;

/// Delays before each attempt to open a failed source again; the last
/// repeats. A device that is gone is tried a few times a minute, and one
/// that comes back (a driver update, Device Manager) is found within
/// seconds.
const REOPEN_DELAYS: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
];

/// Packets the reader may queue ahead of the loop: none, it hands each one
/// over. When the loop falls behind, the sound waits in the driver's ring
/// (which drops its oldest) rather than in a queue that would deliver it
/// late, as when reading and processing were one thread.
const READER_QUEUE: usize = 0;

/// How long a reopened source must stay open without failing before the
/// window says the sound is back (when no sound has come yet).
const BACK_AFTER: Duration = Duration::from_secs(3);

/// The loop's timings, shorter in tests.
struct Timing {
    reopen_delays: &'static [Duration],
    back_after: Duration,
}

const TIMING: Timing = Timing { reopen_delays: REOPEN_DELAYS, back_after: BACK_AFTER };

/// The loop thread's scheduling class while it runs: the "Pro Audio" class
/// the driver read has too (the source sets that on the reader), so the
/// processing doesn't fall behind under load as it would at normal
/// priority. Set and reverted on the thread that runs the loop.
struct ProAudio {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl ProAudio {
    fn for_this_thread() -> Self {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
            let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
            let mut task_index: u32 = 0;
            // SAFETY: a NUL-terminated wide string and a valid u32 pointer.
            let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_index) };
            if handle.is_null() {
                warn!("audio loop: couldn't give the loop thread the Pro Audio priority");
            }
            Self { handle }
        }
        #[cfg(not(windows))]
        Self {}
    }
}

impl Drop for ProAudio {
    fn drop(&mut self) {
        #[cfg(windows)]
        if !self.handle.is_null() {
            // SAFETY: obtained from AvSetMmThreadCharacteristicsW on this thread.
            unsafe { windows_sys::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.handle) };
        }
    }
}

/// 10 ms of silence per packet, the length the driver's stop marker has.
const SILENCE_FRAMES: usize = WIRE_SAMPLE_RATE as usize / 100;

/// What the reader hands the loop.
enum FromReader {
    Packet(AudioPacket),
    /// The source failed (why); the reader is opening it again.
    Lost(String),
    /// A reopened source is being read.
    Reopened,
}

/// Run the audio loop until shutdown. `source` is read on a thread of its
/// own and dropped there (releasing its handles); `reopen` replaces it when
/// it fails.
pub fn run(app: Arc<App>, source: Box<dyn AudioSource>, reopen: Reopen) -> Result<()> {
    run_with(app, source, reopen, &TIMING)
}

fn run_with(app: Arc<App>, source: Box<dyn AudioSource>, reopen: Reopen, timing: &'static Timing) -> Result<()> {
    info!("audio loop: source = {}", source.name());
    let (tx, rx) = bounded::<FromReader>(READER_QUEUE);
    let reader_app = app.clone();
    std::thread::Builder::new()
        .name("stream-to-speaker-audio-read".to_string())
        .spawn(move || read_source(&reader_app, source, reopen, timing.reopen_delays, &tx))
        .context("spawning the audio reader thread")?;

    let _priority = ProAudio::for_this_thread();
    let mut rx = rx;
    let mut out = Output::new(&app);
    let mut feed = Feed::new(Instant::now());
    let mut back = BackCheck::new(timing.back_after);
    while !app.is_shutting_down() {
        let pace = Duration::from_millis(app.silence_pace_ms.load(Ordering::Relaxed).max(1));
        let wake = match back.wake_at() {
            Some(at) => at.min(feed.wake_at()),
            None => feed.wake_at(),
        };
        let got = rx.recv_deadline(wake);
        match got {
            Ok(FromReader::Packet(mut pkt)) => {
                if is_stream_stop(&pkt) {
                    // The driver completes the read it holds with nothing
                    // when Windows stops the stream.
                    debug!("audio: stream stopped; sending silence until it starts again");
                    let _ = feed.source_stopped(Instant::now(), pace);
                } else {
                    let handover = feed.source_packet(Instant::now(), pkt.frame_count(), pace);
                    if handover.fill > 0 {
                        out.publish(&app, AudioPacket::silence(handover.fill), false);
                    }
                    if handover.trim > 0 {
                        let cut = (handover.trim * pkt.channels as usize).min(pkt.samples.len());
                        pkt.samples.drain(..cut);
                    }
                    if !pkt.samples.is_empty() {
                        out.publish(&app, pkt, true);
                    }
                    back.sound();
                }
            }
            Ok(FromReader::Lost(why)) => {
                back.lost();
                app.note_sound_input_lost(&why);
            }
            Ok(FromReader::Reopened) => back.reopened(Instant::now()),
            Err(RecvTimeoutError::Timeout) => {
                if feed.silence_due(Instant::now(), pace) {
                    out.publish(&app, AudioPacket::silence(SILENCE_FRAMES), false);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if app.is_shutting_down() {
                    break;
                }
                // The reader only ends at shutdown; keep the silence going.
                back.lost();
                app.note_sound_input_lost("the audio reader stopped");
                rx = crossbeam_channel::never();
            }
        }
        if back.is_back(Instant::now()) {
            back.told();
            app.note_sound_input_back();
        }
    }
    info!("audio loop: exiting");
    Ok(())
}

/// The driver's "stream stopped": a read completed with no audio, which
/// the source turns into a packet of zeros marked silent.
fn is_stream_stop(pkt: &AudioPacket) -> bool {
    !pkt.samples.is_empty()
        && pkt.flags & PACKET_FLAG_HINT_SILENT != 0
        && pkt.samples.iter().all(|&s| s == 0)
}

/// The reader thread: hand every packet over; when the source fails, drop
/// it, then open it again until that works or the app shuts down. The
/// delays grow across failures until a source delivers audio, so one that
/// opens but fails at once isn't reopened every second.
fn read_source(
    app: &App,
    mut source: Box<dyn AudioSource>,
    mut reopen: Reopen,
    delays: &[Duration],
    tx: &Sender<FromReader>,
) {
    let mut attempts = 0;
    loop {
        let failure = loop {
            match source.recv_packet() {
                Ok(pkt) => {
                    attempts = 0;
                    if tx.send(FromReader::Packet(pkt)).is_err() {
                        return;
                    }
                }
                Err(e) => break e,
            }
        };
        if app.is_shutting_down() {
            return;
        }
        if tx.send(FromReader::Lost(format!("{:#}", failure))).is_err() {
            return;
        }
        // Closed before the next one opens: the driver gives each block of
        // audio to one reader only.
        drop(source);
        source = match open_again_after(app, delays, &mut attempts, "the audio source", &mut reopen) {
            Some(s) => s,
            None => return,
        };
        info!("audio loop: source opened again ({})", source.name());
        if tx.send(FromReader::Reopened).is_err() {
            return;
        }
    }
}

/// Open `what` (something of the sound device's that failed) again with
/// `open`, after a growing delay, until that works: what it opened, or None
/// if the app shuts down first. `attempts` counts on across failures; reset
/// it once what was opened works, so one that opens but fails at once
/// waits longer each time.
pub fn open_again<T>(
    app: &App,
    attempts: &mut usize,
    what: &str,
    open: impl FnMut() -> Result<T>,
) -> Option<T> {
    open_again_after(app, REOPEN_DELAYS, attempts, what, open)
}

/// [`open_again`] after each of `delays` in turn, the last repeating,
/// counting on from `attempts` made before (and counting these).
fn open_again_after<T>(
    app: &App,
    delays: &[Duration],
    attempts: &mut usize,
    what: &str,
    mut open: impl FnMut() -> Result<T>,
) -> Option<T> {
    loop {
        let delay = delays[(*attempts).min(delays.len() - 1)];
        if !app.sleep_unless_shutdown(delay) {
            return None;
        }
        *attempts += 1;
        match open() {
            Ok(opened) => return Some(opened),
            Err(e) => debug!("{}: still can't open it (attempt {}): {:#}", what, attempts, e),
        }
    }
}

/// Everything between a packet and the hub: equalizer, silence handling
/// and the clock-skew trim. (A speaker's delay changes are applied to the
/// stream of the speaker they belong to: see `delay::catch_up`.)
struct Output {
    silence: SilenceDetector,
    eq: Equalizer,
    /// Rate-fudge accumulator for clock-skew compensation.
    fudge_accum: f64,
    last_packet_log: Instant,
    packet_count_since_log: u64,
}

impl Output {
    fn new(app: &App) -> Self {
        Self {
            silence: SilenceDetector::new(
                app.config.silence_packets_threshold,
                !app.config.no_silence_injection,
            ),
            // A no-op until a curve is enabled; new curves arrive from
            // `app.eq` and are crossfaded in.
            eq: Equalizer::new(WIRE_SAMPLE_RATE, WIRE_CHANNELS),
            fudge_accum: 0.0,
            last_packet_log: Instant::now(),
            packet_count_since_log: 0,
        }
    }

    /// Process `pkt` and publish it. `from_source`: it came from Windows
    /// rather than standing in for it.
    fn publish(&mut self, app: &App, mut pkt: AudioPacket, from_source: bool) {
        if pkt.flags & PACKET_FLAG_STREAM_RESTART != 0 {
            info!("stream restart from source (new session)");
        }

        if pkt.sample_rate != WIRE_SAMPLE_RATE || pkt.channels != WIRE_CHANNELS {
            warn!(
                "unexpected source format: rate={} ch={}; expected {}/{}",
                pkt.sample_rate, pkt.channels, WIRE_SAMPLE_RATE, WIRE_CHANNELS,
            );
            return;
        }
        if !from_source {
            app.idle_silence_packets.fetch_add(1, Ordering::Relaxed);
        }

        // EQ before the silence detector, so the keep-alive noise floor
        // it injects is never boosted.
        if let Some(design) = app.eq.take_ready() {
            self.eq.set_design(design);
        }
        self.eq.process(&mut pkt.samples);

        self.silence.process(&mut pkt);
        if from_source {
            app.note_source_packet(self.silence.heard_sound());
        }

        // Rate-fudge: insert / drop a frame when the accumulator crosses ±1.0.
        let fudge_ppm = app.rate_fudge_ppm.load(Ordering::Relaxed);
        if fudge_ppm != 0 && !pkt.samples.is_empty() {
            let frames_in_pkt = pkt.samples.len() / pkt.channels as usize;
            self.fudge_accum += (frames_in_pkt as f64) * (fudge_ppm as f64) * 1e-6;
            let ch = pkt.channels as usize;
            if self.fudge_accum >= 1.0 {
                self.fudge_accum -= 1.0;
                let n = pkt.samples.len();
                if n >= ch {
                    let mut last = Vec::with_capacity(ch);
                    last.extend_from_slice(&pkt.samples[n - ch..]);
                    pkt.samples.extend_from_slice(&last);
                }
            } else if self.fudge_accum <= -1.0 {
                self.fudge_accum += 1.0;
                let n = pkt.samples.len();
                if n >= ch {
                    pkt.samples.truncate(n - ch);
                }
            }
        }

        app.mix_test_sound(&mut pkt.samples, pkt.channels);
        let bytes = samples_to_wav_le_bytes(&pkt.samples);
        app.hub.publish(PcmFrame(Arc::new(bytes)));
        app.packets_published_total.fetch_add(1, Ordering::Relaxed);

        self.packet_count_since_log += 1;
        if self.last_packet_log.elapsed() >= Duration::from_secs(10) {
            debug!(
                "{} packets streamed in last 10s, {} subscriber(s)",
                self.packet_count_since_log,
                app.hub.subscriber_count(),
            );
            self.packet_count_since_log = 0;
            self.last_packet_log = Instant::now();
        }
    }
}

/// How long the source may be quiet before silence is published in its
/// place. While Windows plays, the driver hands over audio every couple of
/// milliseconds, and when this side falls behind it holds the audio back
/// in its 32 KiB ring (186 ms of sound), no longer. A quiet spell this long
/// therefore means the stream stopped: the silence never stands in for
/// audio still to come, which would arrive on top of it and add its length
/// to a pulling speaker's delay.
const QUIET_BEFORE_SILENCE: Duration = Duration::from_millis(200);

/// A silence schedule this far behind the clock (the PC slept, the thread
/// was starved) starts again from now instead of catching up in a burst:
/// a stall that long may have run a pulling speaker dry, and silence made
/// up after that only adds to its delay.
const SILENCE_MAX_BEHIND: Duration = QUIET_BEFORE_SILENCE;

/// What goes out with the first packet from the source after silence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Handover {
    /// Frames of silence to send first.
    fill: usize,
    /// Frames to leave out from the start of the packet.
    trim: usize,
}

/// When the window may say this PC's sound is back after the source
/// failed: once sound comes from it, or once the reopened source has stayed
/// open for `after` without failing. Not as soon as it opens: a device that
/// opens and fails at its first read would make the window flicker; and
/// not only on sound: the driver delivers nothing while Windows plays
/// nothing, and the window would go on saying it is reconnecting.
#[derive(Clone, Copy, Debug)]
struct BackCheck {
    after: Duration,
    state: BackState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackState {
    /// Nothing to report: the source works.
    Fine,
    /// Failed, not open again yet.
    Lost,
    /// Open again since then.
    Reopened(Instant),
    /// Back: sound came, or it stayed open long enough.
    Back,
}

impl BackCheck {
    fn new(after: Duration) -> Self {
        Self { after, state: BackState::Fine }
    }

    fn lost(&mut self) {
        self.state = BackState::Lost;
    }

    fn reopened(&mut self, now: Instant) {
        if self.state == BackState::Lost {
            self.state = BackState::Reopened(now);
        }
    }

    fn sound(&mut self) {
        if matches!(self.state, BackState::Lost | BackState::Reopened(_)) {
            self.state = BackState::Back;
        }
    }

    /// Whether the sound counts as back at `now` (and the window should hear
    /// so); `Fine` once it has been told.
    fn is_back(&self, now: Instant) -> bool {
        match self.state {
            BackState::Back => true,
            BackState::Reopened(at) => now >= at + self.after,
            BackState::Fine | BackState::Lost => false,
        }
    }

    /// The window was told.
    fn told(&mut self) {
        self.state = BackState::Fine;
    }

    /// When `is_back` turns true if nothing else happens first.
    fn wake_at(&self) -> Option<Instant> {
        match self.state {
            BackState::Reopened(at) => Some(at + self.after),
            _ => None,
        }
    }
}

/// Whose packet goes out next: the source's, or silence on a schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feed {
    /// Waiting on the source, which last delivered at `last`.
    Source { last: Instant },
    /// The source is quiet. The next silence packet covers the `pace`
    /// before `next` and goes out at `next`, once that time has passed, so
    /// the silence sent never runs ahead of the clock.
    Silence { next: Instant },
}

/// Frames of sound in `d` at the wire rate.
fn frames_in(d: Duration) -> usize {
    (d.as_nanos() * WIRE_SAMPLE_RATE as u128 / 1_000_000_000) as usize
}

/// How long `frames` of sound last at the wire rate.
fn duration_of(frames: usize) -> Duration {
    Duration::from_nanos((frames as u128 * 1_000_000_000 / WIRE_SAMPLE_RATE as u128) as u64)
}

impl Feed {
    /// At start, as if the source had just delivered: silence follows if
    /// it stays quiet.
    fn new(now: Instant) -> Self {
        Feed::Source { last: now }
    }

    /// When something is due if no packet arrives first.
    fn wake_at(&self) -> Instant {
        match *self {
            Feed::Source { last } => last + QUIET_BEFORE_SILENCE,
            Feed::Silence { next } => next,
        }
    }

    /// A packet of `frames` came from the source: it goes out, and silence
    /// waits. Returns what goes with it: after silence, the stretch between
    /// the end of the silence sent and the start of this packet's sound
    /// (its arrival less its length) is filled, or, if the silence already
    /// covers part of this packet's time, that part of the packet is left
    /// out, so the stream keeps to the clock across the hand-over.
    fn source_packet(&mut self, now: Instant, frames: usize, pace: Duration) -> Handover {
        let was = std::mem::replace(self, Feed::Source { last: now });
        let Feed::Silence { next } = was else {
            return Handover::default();
        };
        let silence_ends = next - pace;
        let sound_starts = now.checked_sub(duration_of(frames)).unwrap_or(now);
        if sound_starts >= silence_ends {
            // At the rate silence goes out (one packet per pace).
            let gap = sound_starts - silence_ends;
            let fill = gap.as_nanos() * SILENCE_FRAMES as u128 / pace.as_nanos().max(1);
            Handover { fill: fill as usize, trim: 0 }
        } else {
            Handover { fill: 0, trim: frames_in(silence_ends - sound_starts).min(frames) }
        }
    }

    /// The source says its stream stopped: silence takes over from now on,
    /// its first packet going out once its time has passed. Returns how many
    /// frames of silence go out at once (none).
    fn source_stopped(&mut self, now: Instant, pace: Duration) -> usize {
        if let Feed::Source { .. } = *self {
            *self = Feed::Silence { next: now + pace };
        }
        0
    }

    /// Whether a silence packet is due at `now`; if so it counts as sent.
    fn silence_due(&mut self, now: Instant, pace: Duration) -> bool {
        if let Feed::Source { last } = *self {
            if now < last + QUIET_BEFORE_SILENCE {
                return false;
            }
            *self = Feed::Silence { next: last + QUIET_BEFORE_SILENCE + pace };
        }
        let Feed::Silence { next } = self else { return false };
        if now < *next {
            return false;
        }
        // On the schedule, not from now: a late wake-up is made up by the
        // next packets going out at once, so the silence keeps pace with
        // the clock.
        *next += pace;
        if now.saturating_duration_since(*next) > SILENCE_MAX_BEHIND {
            *next = now + pace;
        }
        true
    }
}

#[cfg(test)]
mod feed_tests {
    use super::*;

    const PACE: Duration = Duration::from_millis(10);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// How many silence packets go out at `now` (more than one when the
    /// loop is behind its schedule).
    fn due_at(feed: &mut Feed, now: Instant) -> usize {
        let mut n = 0;
        while feed.silence_due(now, PACE) {
            n += 1;
            assert!(n < 1000, "silence without end at one instant");
        }
        n
    }

    #[test]
    fn no_silence_between_packets_of_a_running_stream() {
        // Gaps a running stream has: the driver's 2 ms, a late timer, the
        // most its ring can hold back.
        let t0 = Instant::now();
        let mut feed = Feed::new(t0);
        let mut t = t0;
        for gap in [2, 16, 50, 120, 186, 2, 190] {
            assert_eq!(due_at(&mut feed, t + ms(gap)), 0, "silence after a {gap} ms gap");
            t += ms(gap);
            feed.source_packet(t, 88, PACE);
        }
    }

    #[test]
    fn a_quiet_source_gets_silence_at_real_time() {
        let t0 = Instant::now();
        let mut feed = Feed::new(t0);
        feed.source_packet(t0, 88, PACE);
        assert_eq!(due_at(&mut feed, t0 + ms(190)), 0);
        // Ten seconds of waking up 7 ms late every time (a coarse system
        // timer): the silence still adds up to the time that passed.
        let mut published = 0;
        let mut now = t0 + ms(190);
        while now < t0 + Duration::from_secs(10) {
            now = feed.wake_at().max(now) + ms(7);
            published += due_at(&mut feed, now);
        }
        let quiet = now - t0 - QUIET_BEFORE_SILENCE;
        let expected = (quiet.as_millis() / 10) as usize;
        assert!(
            published.abs_diff(expected) <= 1,
            "{published} silence packets for {quiet:?} of quiet (expected {expected})"
        );
    }

    #[test]
    fn a_packet_ends_the_silence_at_once() {
        let t0 = Instant::now();
        let mut feed = Feed::new(t0);
        assert!(due_at(&mut feed, t0 + Duration::from_secs(1)) > 0);
        let t1 = t0 + Duration::from_secs(1) + ms(3);
        feed.source_packet(t1, 88, PACE);
        // A silence packet that was due a moment ago no longer goes out.
        assert_eq!(due_at(&mut feed, t1 + ms(5)), 0);
        assert_eq!(feed.wake_at(), t1 + QUIET_BEFORE_SILENCE);
    }

    #[test]
    fn a_stop_from_the_source_starts_the_silence_straight_away() {
        let t0 = Instant::now();
        let mut feed = Feed::new(t0);
        feed.source_packet(t0, 88, PACE);
        let _ = feed.source_stopped(t0 + ms(1), PACE);
        // The first 10 ms of silence goes out once those 10 ms have passed.
        assert_eq!(due_at(&mut feed, t0 + ms(10)), 0);
        assert_eq!(due_at(&mut feed, t0 + ms(11)), 1);
        // A second stop while silence runs adds nothing to the schedule.
        assert_eq!(feed.source_stopped(t0 + ms(12), PACE), 0);
        assert_eq!(due_at(&mut feed, t0 + ms(21)), 1);
    }

    /// Silence from a driver stop at `t0` up to the first real packet of
    /// `frames` at `t0 + after`, waking `late` after each due time: what
    /// went out, silence and audio together, in frames.
    fn across_a_restart(after: Duration, frames: usize, late: Duration) -> usize {
        let t0 = Instant::now() + Duration::from_secs(1);
        let mut feed = Feed::new(t0 - ms(500));
        let _ = feed.source_packet(t0 - ms(2), 88, PACE);
        let mut out = feed.source_stopped(t0, PACE);
        let t1 = t0 + after;
        let mut now = t0;
        loop {
            let wake = feed.wake_at().max(now) + late;
            if wake > t1 {
                break;
            }
            now = wake;
            while feed.silence_due(now, PACE) {
                out += SILENCE_FRAMES;
            }
        }
        let handover = feed.source_packet(t1, frames, PACE);
        assert!(handover.trim <= frames);
        out + handover.fill + frames - handover.trim
    }

    #[test]
    fn over_a_stop_and_restart_the_stream_keeps_to_the_clock() {
        for frames in [44, 88, 441, 2048] {
            for late in [0, 3, 7] {
                // The first real packet anywhere from at once to 120 ms after
                // the stop, before and after each silence packet's time.
                for step in 0..=480u64 {
                    let after = Duration::from_micros(step * 250);
                    let out = across_a_restart(after, frames, ms(late));
                    let passed = frames_in(after);
                    assert!(
                        out <= passed + 1,
                        "{out} frames went out for {passed} frames of time ({after:?}, {frames}-frame packet, {late} ms late)"
                    );
                    assert!(
                        out + frames_in(ms(1)) >= passed,
                        "{out} frames went out for {passed} frames of time ({after:?}, {frames}-frame packet, {late} ms late)"
                    );
                }
            }
        }
    }

    #[test]
    fn after_a_quiet_spell_the_stream_never_runs_ahead_of_the_clock() {
        for step in 0..=200u64 {
            let t0 = Instant::now() + Duration::from_secs(1);
            let mut feed = Feed::new(t0);
            let _ = feed.source_packet(t0, 88, PACE);
            let t1 = t0 + QUIET_BEFORE_SILENCE + Duration::from_micros(step * 500);
            let mut out = 0;
            let mut now = t0;
            while feed.wake_at() <= t1 {
                now = feed.wake_at().max(now);
                while feed.silence_due(now, PACE) {
                    out += SILENCE_FRAMES;
                }
            }
            let handover = feed.source_packet(t1, 88, PACE);
            let out = out + handover.fill + 88 - handover.trim;
            let passed = frames_in(t1 - t0);
            // Short by the quiet spell before silence started, no more.
            assert!(out <= passed + 1, "{out} > {passed}");
            assert!(out + frames_in(QUIET_BEFORE_SILENCE + ms(1)) >= passed, "{out} for {passed}");
        }
    }

    #[test]
    fn a_long_stall_restarts_the_schedule_instead_of_bursting() {
        let t0 = Instant::now();
        let mut feed = Feed::new(t0);
        assert!(due_at(&mut feed, t0 + ms(250)) > 0);
        // A 30 ms hiccup is made up.
        let n = due_at(&mut feed, t0 + ms(300));
        assert!((4..=6).contains(&n), "{n} packets to make up 30 ms");
        // A stall past the quiet threshold isn't: catching that up would
        // add it to a pulling speaker's delay once its buffer ran dry.
        let n = due_at(&mut feed, t0 + ms(700));
        assert!(n <= 21, "{n} packets after a 400 ms stall");
        let t1 = t0 + Duration::from_secs(60);
        assert_eq!(due_at(&mut feed, t1), 1);
        assert_eq!(due_at(&mut feed, t1 + ms(9)), 0);
        assert_eq!(due_at(&mut feed, t1 + ms(10)), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_server::PcmFrame;
    use anyhow::anyhow;
    use crossbeam_channel::{unbounded, Receiver};
    use std::sync::atomic::AtomicBool;

    /// A source the test feeds: each read returns the test's next item and
    /// waits for it meanwhile (Windows is playing nothing); once the test
    /// drops its end, reads fail. Every read is announced on `reading`, and
    /// `open` is cleared when the source is dropped (its handles closed).
    struct Fed {
        items: Receiver<Result<AudioPacket>>,
        reading: Sender<()>,
        open: Arc<AtomicBool>,
    }

    impl AudioSource for Fed {
        fn recv_packet(&mut self) -> Result<AudioPacket> {
            let _ = self.reading.send(());
            self.items.recv().unwrap_or_else(|_| Err(anyhow!("the test closed the source")))
        }
    }

    impl Drop for Fed {
        fn drop(&mut self) {
            self.open.store(false, Ordering::SeqCst);
        }
    }

    /// The test's ends of a [`Fed`] source.
    struct Feeder {
        items: Sender<Result<AudioPacket>>,
        reads: Receiver<()>,
        open: Arc<AtomicBool>,
    }

    fn fed() -> (Feeder, Box<dyn AudioSource>) {
        let (items_tx, items) = unbounded();
        let (reading, reads) = unbounded();
        let open = Arc::new(AtomicBool::new(true));
        (
            Feeder { items: items_tx, reads, open: open.clone() },
            Box::new(Fed { items, reading, open }),
        )
    }

    /// 10 ms of sound at a level silence handling never touches.
    fn sound() -> AudioPacket {
        AudioPacket {
            samples: vec![1000; 441 * 2],
            sample_rate: WIRE_SAMPLE_RATE,
            channels: WIRE_CHANNELS,
            timestamp_qpc: 0,
            stream_position: 0,
            flags: 0,
        }
    }

    fn first_sample(frame: &PcmFrame) -> i16 {
        i16::from_le_bytes([frame.0[0], frame.0[1]])
    }

    const PATIENCE: Duration = Duration::from_secs(5);

    fn next_frame(hub: &Receiver<PcmFrame>, what: &str) -> PcmFrame {
        hub.recv_timeout(PATIENCE).unwrap_or_else(|_| panic!("no frame within {PATIENCE:?}: {what}"))
    }

    /// Frames until one carries the test's sound.
    fn wait_for_sound(hub: &Receiver<PcmFrame>) {
        let end = Instant::now() + PATIENCE;
        while Instant::now() < end {
            if first_sample(&next_frame(hub, "sound")) == 1000 {
                return;
            }
        }
        panic!("no sound within {PATIENCE:?}");
    }

    fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let end = Instant::now() + PATIENCE;
        while !cond() {
            assert!(Instant::now() < end, "not within {PATIENCE:?}: {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Reopening never works: the device stays gone.
    fn gone() -> Reopen {
        Box::new(|| Err(anyhow!("no such device")))
    }

    const A_MILLISECOND_APART: &[Duration] = &[Duration::from_millis(1)];
    const FAST: Timing = Timing { reopen_delays: A_MILLISECOND_APART, back_after: Duration::from_millis(30) };

    /// Run the loop on a thread, with reopen attempts a millisecond apart;
    /// the receiver hears when it returns.
    fn start(app: &Arc<App>, source: Box<dyn AudioSource>, reopen: Reopen) -> Receiver<()> {
        let (done_tx, done) = unbounded();
        let app = app.clone();
        std::thread::spawn(move || {
            let _ = run_with(app, source, reopen, &FAST);
            let _ = done_tx.send(());
        });
        done
    }

    /// The source delivers sound, then nothing: Windows stopped playing
    /// without the driver's stop reaching the loop, or (WASAPI) nothing
    /// plays. `late_stop`: the driver's StreamStop event arrives once the
    /// loop is already waiting.
    fn quiet_source_gets_silence(late_stop: bool) {
        let app = App::for_tests();
        let hub = app.hub.subscribe();
        let (feeder, source) = fed();
        let _done = start(&app, source, gone());
        for _ in 0..3 {
            feeder.items.send(Ok(sound())).unwrap();
            assert_eq!(first_sample(&next_frame(&hub, "sound")), 1000);
        }
        if late_stop {
            app.stream_active.store(false, Ordering::Release);
        }
        for _ in 0..20 {
            let f = next_frame(&hub, "silence while the source is quiet");
            assert!(first_sample(&f).abs() <= 4, "expected silence, got {}", first_sample(&f));
        }
        // And the next sound ends it.
        feeder.items.send(Ok(sound())).unwrap();
        wait_for_sound(&hub);
        app.request_shutdown();
    }

    #[test]
    fn silence_goes_out_while_windows_plays_nothing() {
        quiet_source_gets_silence(false);
    }

    /// A speaker that connects before anything has played on the PC gets a
    /// stream from the start (silence, at the pace of the clock), so it
    /// keeps its fetch open and a delay change applies to it at once.
    #[test]
    fn a_speaker_connected_before_anything_plays_gets_silence() {
        let app = App::for_tests();
        let hub = app.hub.subscribe();
        let (feeder, source) = fed();
        let _done = start(&app, source, gone());
        for _ in 0..20 {
            let f = next_frame(&hub, "silence before the first play");
            assert_eq!(f.0.len(), SILENCE_FRAMES * 4);
            assert!(first_sample(&f).abs() <= 4, "expected silence, got {}", first_sample(&f));
        }
        // The first sound takes over.
        feeder.items.send(Ok(sound())).unwrap();
        wait_for_sound(&hub);
        app.request_shutdown();
    }

    #[test]
    fn silence_goes_out_after_a_stop_the_loop_learns_late() {
        quiet_source_gets_silence(true);
    }

    #[test]
    fn the_loop_ends_at_shutdown_while_the_source_waits() {
        let app = App::for_tests();
        let (feeder, source) = fed();
        let done = start(&app, source, gone());
        feeder.reads.recv_timeout(PATIENCE).expect("the loop never read");
        app.request_shutdown();
        assert!(done.recv_timeout(PATIENCE).is_ok(), "still running {PATIENCE:?} after shutdown");
    }

    #[test]
    fn a_failing_source_leaves_the_speaker_with_silence_not_nothing() {
        let app = App::for_tests();
        let hub = app.hub.subscribe();
        let (feeder, source) = fed();
        let _done = start(&app, source, gone());
        feeder.items.send(Ok(sound())).unwrap();
        next_frame(&hub, "sound");
        feeder.items.send(Err(anyhow!("WinError 995"))).unwrap();
        for _ in 0..20 {
            next_frame(&hub, "silence after the source failed");
        }
        assert!(app.sound_input_lost(), "the window isn't told");
        app.request_shutdown();
    }

    #[test]
    fn a_source_that_fails_as_soon_as_it_opens_waits_longer_each_time() {
        let app = App::for_tests();
        let (first, source) = fed();
        first.items.send(Err(anyhow!("WinError 995"))).unwrap();
        // Every reopened source fails at its first read, before any audio.
        let reopen: Reopen = Box::new(|| {
            let (feeder, source) = fed();
            feeder.items.send(Err(anyhow!("WinError 995"))).unwrap();
            Ok(source)
        });
        const SCHEDULE: &[Duration] =
            &[Duration::from_millis(1), Duration::from_millis(60), Duration::from_millis(120)];
        let (tx, rx) = bounded(READER_QUEUE);
        let reader = {
            let app = app.clone();
            std::thread::spawn(move || read_source(&app, source, reopen, SCHEDULE, &tx))
        };
        let mut lost_at = Vec::new();
        while lost_at.len() < 4 {
            if let FromReader::Lost(_) = rx.recv_timeout(PATIENCE).expect("the reader went quiet") {
                lost_at.push(Instant::now());
            }
        }
        app.request_shutdown();
        drop(rx);
        reader.join().unwrap();
        // Between losses the reader waited the schedule's next delay, not
        // its first one again.
        let gaps: Vec<Duration> = lost_at.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps[1] >= SCHEDULE[1], "second wait {:?}", gaps[1]);
        assert!(gaps[2] >= SCHEDULE[2], "third wait {:?}", gaps[2]);
    }

    #[test]
    fn a_stalled_loop_leaves_the_sound_with_the_driver() {
        // Ten packets ready, and nobody taking them: the reader may hold one,
        // as the single thread did, and the rest stays where the driver keeps
        // it (dropping its oldest), not in a queue that would deliver it late.
        let app = App::for_tests();
        let (feeder, source) = fed();
        for _ in 0..10 {
            feeder.items.send(Ok(sound())).unwrap();
        }
        let (tx, rx) = bounded(READER_QUEUE);
        let reader = {
            let app = app.clone();
            std::thread::spawn(move || read_source(&app, source, gone(), A_MILLISECOND_APART, &tx))
        };
        feeder.reads.recv_timeout(PATIENCE).expect("never read");
        std::thread::sleep(Duration::from_millis(100));
        assert!(feeder.reads.try_recv().is_err(), "read ahead of a loop that took nothing");
        // Taking one lets exactly one more read happen.
        assert!(matches!(rx.recv_timeout(PATIENCE), Ok(FromReader::Packet(_))));
        feeder.reads.recv_timeout(PATIENCE).expect("no read after the hand-over");
        app.request_shutdown();
        drop(rx);
        drop(feeder);
        reader.join().unwrap();
    }

    #[test]
    fn the_window_hears_the_sound_is_back_once_it_is() {
        let t0 = Instant::now();
        let after = Duration::from_millis(100);
        let mut back = BackCheck::new(after);
        back.lost();
        back.reopened(t0);
        // Opened, but neither sound nor time yet.
        assert!(!back.is_back(t0 + after / 2));
        // Sound: back at once.
        let mut sounding = back;
        sounding.sound();
        assert!(sounding.is_back(t0));
        // Open and failing at once, again and again: never back.
        let mut failing = back;
        failing.lost();
        failing.reopened(t0 + after / 2);
        failing.lost();
        assert!(!failing.is_back(t0 + after * 10));
        // Open and simply quiet (Windows plays nothing): back after a while.
        assert!(back.is_back(t0 + after));
        assert_eq!(back.wake_at(), Some(t0 + after));
    }

    #[test]
    fn a_device_that_stays_gone_is_tried_a_few_times_a_minute() {
        let mut elapsed = Duration::ZERO;
        let mut attempts = 0;
        for i in 0.. {
            elapsed += REOPEN_DELAYS[i.min(REOPEN_DELAYS.len() - 1)];
            if elapsed > Duration::from_secs(60) {
                break;
            }
            attempts += 1;
        }
        assert!(REOPEN_DELAYS[0] <= Duration::from_secs(2), "the first retry waits too long");
        assert!((4..=10).contains(&attempts), "{attempts} attempts in the first minute");
        assert!(*REOPEN_DELAYS.last().unwrap() <= Duration::from_secs(30), "a returning device is found late");
    }

    #[test]
    fn a_failed_source_is_opened_again_once_the_old_one_is_closed() {
        let app = App::for_tests();
        let hub = app.hub.subscribe();
        let (first, source) = fed();
        let (second, second_source) = fed();
        let first_open = first.open.clone();
        let (attempts_tx, attempts) = unbounded();
        let (back_tx, back) = unbounded::<()>();
        let mut second_source = Some(second_source);
        let mut failures_left = 2;
        // Fails twice, then opens once the test lets the device come back.
        let reopen: Reopen = Box::new(move || {
            let _ = attempts_tx.send(first_open.load(Ordering::SeqCst));
            if failures_left > 0 {
                failures_left -= 1;
                return Err(anyhow!("not back yet"));
            }
            let _ = back.recv();
            Ok(second_source.take().expect("opened once"))
        });
        let _done = start(&app, source, reopen);
        first.items.send(Ok(sound())).unwrap();
        wait_for_sound(&hub);

        first.items.send(Err(anyhow!("WinError 995"))).unwrap();
        for attempt in 1..=3 {
            let first_still_open = attempts.recv_timeout(PATIENCE).expect("no attempt to open it again");
            assert!(!first_still_open, "attempt {attempt} while the failed source was still open");
        }
        wait_until("the window hears the sound is lost", || app.sound_input_lost());
        next_frame(&hub, "silence while the device is away");
        back_tx.send(()).unwrap();
        second.reads.recv_timeout(PATIENCE).expect("the reopened source is never read");
        wait_until("the window hears the sound is back", || !app.sound_input_lost());
        second.items.send(Ok(sound())).unwrap();
        wait_for_sound(&hub);
        app.request_shutdown();
    }
}
