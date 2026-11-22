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
use crate::http_server::{samples_to_l16_be_bytes, PcmFrame};
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

/// Packets the reader may queue ahead of the loop.
const READER_QUEUE: usize = 128;

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
    run_with(app, source, reopen, REOPEN_DELAYS)
}

fn run_with(
    app: Arc<App>,
    source: Box<dyn AudioSource>,
    reopen: Reopen,
    reopen_delays: &'static [Duration],
) -> Result<()> {
    info!("audio loop: source = {}", source.name());
    let (tx, rx) = bounded::<FromReader>(READER_QUEUE);
    let reader_app = app.clone();
    std::thread::Builder::new()
        .name("stream-to-speaker-audio-read".to_string())
        .spawn(move || read_source(&reader_app, source, reopen, reopen_delays, &tx))
        .context("spawning the audio reader thread")?;

    let mut rx = rx;
    let mut out = Output::new(&app);
    let mut feed = Feed::new(Instant::now());
    while !app.is_shutting_down() {
        let pace = Duration::from_millis(app.silence_pace_ms.load(Ordering::Relaxed).max(1));
        match rx.recv_deadline(feed.wake_at()) {
            Ok(FromReader::Packet(pkt)) => {
                if is_stream_stop(&pkt) {
                    // The driver completes the read it holds with nothing
                    // when Windows stops the stream.
                    if feed.source_stopped(Instant::now(), pace) {
                        debug!("audio: stream stopped; sending silence until it starts again");
                        out.publish(&app, pkt, false);
                    }
                } else {
                    feed.source_packet(Instant::now());
                    out.publish(&app, pkt, true);
                }
            }
            Ok(FromReader::Lost(why)) => app.note_sound_input_lost(&why),
            Ok(FromReader::Reopened) => app.note_sound_input_back(),
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
                app.note_sound_input_lost("the audio reader stopped");
                rx = crossbeam_channel::never();
            }
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

/// Everything between a packet and the hub: equalizer, silence handling,
/// clock-skew and latency trims.
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

        // Runtime latency-adjust: drop / duplicate up to N frames per packet.
        if !pkt.samples.is_empty() {
            let pending = app.drain_frames.load(Ordering::Acquire);
            if pending != 0 {
                let ch = pkt.channels as usize;
                let max_step =
                    app.latency_adjust_step_frames.load(Ordering::Relaxed).max(1) as i64;
                let step = pending.signum() * pending.abs().min(max_step);
                if step > 0 {
                    let drop_samples = (step as usize) * ch;
                    let new_len = pkt.samples.len().saturating_sub(drop_samples);
                    pkt.samples.truncate(new_len);
                    app.drain_frames.fetch_sub(step, Ordering::AcqRel);
                } else if step < 0 && pkt.samples.len() >= ch {
                    let start = pkt.samples.len() - ch;
                    let last_frame: Vec<i16> = pkt.samples[start..].to_vec();
                    for _ in 0..(-step) {
                        pkt.samples.extend_from_slice(&last_frame);
                    }
                    app.drain_frames.fetch_sub(step, Ordering::AcqRel);
                }
            }
        }

        let bytes = samples_to_l16_be_bytes(&pkt.samples);
        app.hub.publish(PcmFrame(Arc::new(bytes)));
        app.packets_published_total.fetch_add(1, Ordering::Relaxed);

        self.packet_count_since_log += 1;
        if self.last_packet_log.elapsed() >= Duration::from_secs(10) {
            debug!(
                "{} packets streamed in last 10s, {} subscriber(s), pending latency-adjust {} frames",
                self.packet_count_since_log,
                app.hub.subscriber_count(),
                app.drain_frames.load(Ordering::Relaxed),
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
/// was starved) starts again from now instead of catching up in a burst.
const SILENCE_MAX_BEHIND: Duration = Duration::from_secs(1);

/// Whose packet goes out next: the source's, or silence on a schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feed {
    /// Waiting on the source, which last delivered at `last`.
    Source { last: Instant },
    /// The source is quiet; the next silence packet is due at `next`.
    Silence { next: Instant },
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

    /// A packet came from the source: it goes out, and silence waits.
    fn source_packet(&mut self, now: Instant) {
        *self = Feed::Source { last: now };
    }

    /// The source says its stream stopped, handing over a packet of
    /// silence. Returns whether that packet goes out now (it does unless
    /// silence is already on its schedule).
    fn source_stopped(&mut self, now: Instant, pace: Duration) -> bool {
        match *self {
            Feed::Source { .. } => {
                *self = Feed::Silence { next: now + pace };
                true
            }
            Feed::Silence { .. } => false,
        }
    }

    /// Whether a silence packet is due at `now`; if so it counts as sent.
    fn silence_due(&mut self, now: Instant, pace: Duration) -> bool {
        if let Feed::Source { last } = *self {
            if now < last + QUIET_BEFORE_SILENCE {
                return false;
            }
            *self = Feed::Silence { next: last + QUIET_BEFORE_SILENCE };
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
            feed.source_packet(t);
        }
    }

    #[test]
    fn a_quiet_source_gets_silence_at_real_time() {
        let t0 = Instant::now();
        let mut feed = Feed::new(t0);
        feed.source_packet(t0);
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
        feed.source_packet(t1);
        // A silence packet that was due a moment ago no longer goes out.
        assert_eq!(due_at(&mut feed, t1 + ms(5)), 0);
        assert_eq!(feed.wake_at(), t1 + QUIET_BEFORE_SILENCE);
    }

    #[test]
    fn a_stop_from_the_source_starts_the_silence_straight_away() {
        let t0 = Instant::now();
        let mut feed = Feed::new(t0);
        feed.source_packet(t0);
        assert!(feed.source_stopped(t0 + ms(1), PACE), "the stop's own silence goes out");
        assert_eq!(due_at(&mut feed, t0 + ms(10)), 0);
        assert_eq!(due_at(&mut feed, t0 + ms(11)), 1);
        // A second stop while silence runs adds nothing to the schedule.
        assert!(!feed.source_stopped(t0 + ms(12), PACE));
        assert_eq!(due_at(&mut feed, t0 + ms(21)), 1);
    }

    #[test]
    fn a_long_stall_restarts_the_schedule_instead_of_bursting() {
        let t0 = Instant::now();
        let mut feed = Feed::new(t0);
        assert!(due_at(&mut feed, t0 + ms(250)) > 0);
        // A 30 ms hiccup is made up; the PC sleeping for a minute isn't.
        let n = due_at(&mut feed, t0 + ms(300));
        assert!((4..=6).contains(&n), "{n} packets to make up 30 ms");
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

    /// Run the loop on a thread, with reopen attempts a millisecond apart;
    /// the receiver hears when it returns.
    fn start(app: &Arc<App>, source: Box<dyn AudioSource>, reopen: Reopen) -> Receiver<()> {
        let (done_tx, done) = unbounded();
        let app = app.clone();
        std::thread::spawn(move || {
            let _ = run_with(app, source, reopen, A_MILLISECOND_APART);
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
