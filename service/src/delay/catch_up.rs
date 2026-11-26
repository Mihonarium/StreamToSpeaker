//! The gradual catch-up of a UPnP speaker's stream.
//!
//! A Sonos or other UPnP speaker fetches the stream this PC serves and
//! keeps some of it in its own buffer before playing it: that buffer is
//! its delay. Removing delay drops single frames from what the speaker is
//! sent, a few per packet, until it has been sent that many frames less
//! than the time that passed; its buffer then holds that much less. Adding
//! repeats frames the same way. Each of the speaker's fetches counts on
//! its own: a fetch starts with the speaker's own delay (it fills a fresh
//! buffer), so the whole change is applied again on it.
//!
//! While the stream carries silence (nothing plays yet, or a pause), the
//! change applies at once: frames of silence can go or come without
//! anything to hear. A change made before the first play is in place by
//! the time the sound starts.
//!
//! The person's change is held as a target; each fetch's [`Listener`]
//! counts what it has applied so far. A speaker that runs dry soon after
//! delay was removed (it fetches again, or its transport says it is
//! buffering) is reported as a cut-out (see [`CatchUp::listener`]); the
//! app never changes the delay itself.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{frames_to_ms, ms_to_frames};

/// A sample this close to zero is silence (the stream's keep-alive noise
/// floor is a few steps).
const SILENT_LEVEL: i16 = 16;
/// The most frames added to one packet at once (100 ms).
const MAX_ADDED_AT_ONCE: usize = 4410;
/// Bytes per frame on the wire: two 16-bit channels.
const FRAME_BYTES: usize = 4;
/// A fetch that delivered this much before the next one came was playing;
/// one that ended sooner was the speaker probing the stream.
const PLAYED_BEFORE_REFETCH: Duration = Duration::from_secs(2);
/// A speaker that runs dry this soon after delay was removed cut out
/// because of it.
pub const DRY_AFTER_REMOVE: Duration = Duration::from_secs(30);

/// The speaker's sound cut out soon after delay was removed; `removed_ms`
/// is what was removed then. Nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutOut {
    pub removed_ms: u32,
}

/// The bound UPnP speaker's catch-up, shared by the fetches serving it.
pub struct CatchUp {
    /// The change to reach, in ms (negative: removed).
    target_ms: AtomicI32,
    /// Frames per packet the catch-up moves while sound plays.
    step: Arc<AtomicU32>,
    /// Apply a change at once, with a short skip, instead of gradually.
    at_once: AtomicBool,
    inner: Mutex<Inner>,
    /// Told of each cut-out.
    on_cut_out: Mutex<Option<Box<dyn Fn(CutOut) + Send + Sync>>>,
}

#[derive(Default)]
struct Inner {
    /// The newest fetch, if the speaker has fetched since its session began.
    current: Option<Current>,
    next_listener: u64,
    /// When frames were last removed.
    last_removed: Option<Instant>,
    /// Counts the times the speaker refilled its buffer without fetching
    /// again: every fetch then starts counting from nothing.
    rebuffers: u64,
}

/// What the newest fetch has done.
#[derive(Clone, Copy)]
struct Current {
    number: u64,
    delivered_frames: u64,
    applied_frames: i64,
    dropped_frames: u64,
}

impl CatchUp {
    /// A catch-up moving at most `step` frames per packet while sound
    /// plays.
    pub fn new(step: Arc<AtomicU32>) -> Arc<Self> {
        Arc::new(Self {
            target_ms: AtomicI32::new(0),
            step,
            at_once: AtomicBool::new(false),
            inner: Mutex::new(Inner::default()),
            on_cut_out: Mutex::new(None),
        })
    }

    /// Call `f` at each cut-out (see [`Self::listener`]).
    pub fn on_cut_out(&self, f: impl Fn(CutOut) + Send + Sync + 'static) {
        *self.on_cut_out.lock().unwrap() = Some(Box::new(f));
    }

    /// A speaker's session begins with `change_ms`: nothing is applied
    /// for it yet.
    pub fn begin_session(&self, change_ms: i32) {
        let mut inner = self.inner.lock().unwrap();
        inner.current = None;
        inner.last_removed = None;
        self.target_ms.store(change_ms, Ordering::Release);
    }

    /// The change to reach is now `change_ms`.
    pub fn set_target(&self, change_ms: i32) {
        self.target_ms.store(change_ms, Ordering::Release);
    }

    pub fn target_ms(&self) -> i32 {
        self.target_ms.load(Ordering::Acquire)
    }

    /// Apply changes at once, with a short skip, instead of gradually.
    pub fn set_at_once(&self, on: bool) {
        self.at_once.store(on, Ordering::Release);
    }

    /// What is still to apply, ms: negative while removing, positive while
    /// adding. All of it until the speaker fetches the stream.
    pub fn ms_to_go(&self) -> i32 {
        let applied = self.inner.lock().unwrap().current.map_or(0, |c| c.applied_frames);
        self.target_ms() - frames_to_ms(applied) as i32
    }

    /// Frames the stream couldn't queue for the speaker's newest fetch
    /// because it wasn't reading.
    pub fn dropped_frames(&self) -> u64 {
        self.inner.lock().unwrap().current.map_or(0, |c| c.dropped_frames)
    }

    /// A new fetch by the speaker. The speaker starts it with its own
    /// delay, so the whole change applies again on it. If a fetch that had
    /// been playing came before it while delay was removed recently, the
    /// speaker ran dry: a cut-out.
    pub fn listener(self: &Arc<Self>) -> Listener {
        let (cut_out, listener) = {
            let mut inner = self.inner.lock().unwrap();
            let ran_dry = inner.current.is_some_and(|c| c.delivered_frames >= played_frames());
            let cut_out = if ran_dry { self.cut_out_if_recent(&inner) } else { None };
            inner.next_listener += 1;
            let number = inner.next_listener;
            inner.current = Some(Current { number, delivered_frames: 0, applied_frames: 0, dropped_frames: 0 });
            let rebuffers_seen = inner.rebuffers;
            (cut_out, Listener { up: self.clone(), number, applied_frames: 0, delivered_frames: 0, rebuffers_seen })
        };
        self.told(cut_out);
        listener
    }

    /// The speaker refilled its buffer on the same fetch (its transport
    /// went through TRANSITIONING): it is back at its own delay, so the
    /// change applies again; a cut-out as for a new fetch.
    pub fn speaker_rebuffered(&self) {
        let cut_out = {
            let mut inner = self.inner.lock().unwrap();
            let playing = inner.current.is_some_and(|c| c.delivered_frames >= played_frames());
            let cut_out = if playing { self.cut_out_if_recent(&inner) } else { None };
            inner.rebuffers += 1;
            if let Some(c) = inner.current.as_mut() {
                c.applied_frames = 0;
            }
            cut_out
        };
        self.told(cut_out);
    }

    /// The speaker was told to start over (Stop and Play): like a refill,
    /// without a cut-out, since nobody's change ran it dry.
    pub fn restarted(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.rebuffers += 1;
        inner.last_removed = None;
        if let Some(c) = inner.current.as_mut() {
            c.applied_frames = 0;
        }
    }

    /// A cut-out, if delay is removed and frames were removed within
    /// [`DRY_AFTER_REMOVE`].
    fn cut_out_if_recent(&self, inner: &Inner) -> Option<CutOut> {
        let change_ms = self.target_ms();
        let recent = inner.last_removed.is_some_and(|t| t.elapsed() <= DRY_AFTER_REMOVE);
        (change_ms < 0 && recent).then_some(CutOut { removed_ms: change_ms.unsigned_abs() })
    }

    fn told(&self, cut_out: Option<CutOut>) {
        if let Some(c) = cut_out {
            log::info!("delay: the speaker ran dry with {} ms removed", c.removed_ms);
            if let Some(f) = self.on_cut_out.lock().unwrap().as_ref() {
                f(c);
            }
        }
    }
}

fn played_frames() -> u64 {
    (PLAYED_BEFORE_REFETCH.as_millis() as u64) * crate::WIRE_SAMPLE_RATE as u64 / 1000
}

/// One fetch of the stream by the speaker: what it has been sent so far
/// against the change it should have.
pub struct Listener {
    up: Arc<CatchUp>,
    number: u64,
    applied_frames: i64,
    delivered_frames: u64,
    rebuffers_seen: u64,
}

impl Listener {
    /// Bring one packet of the stream (16-bit stereo, little-endian) a
    /// step closer to the change: drop frames from its end, or repeat its
    /// last frame. A packet may end up empty.
    pub fn edit(&mut self, bytes: &mut Vec<u8>) {
        let mut inner = self.up.inner.lock().unwrap();
        if inner.rebuffers != self.rebuffers_seen {
            self.rebuffers_seen = inner.rebuffers;
            self.applied_frames = 0;
        }
        let pending = ms_to_frames(self.up.target_ms() as i64) - self.applied_frames;
        let frames = bytes.len() / FRAME_BYTES;
        if pending != 0 && frames > 0 {
            let whole = self.up.at_once.load(Ordering::Acquire) || is_silent(bytes);
            let step = self.up.step.load(Ordering::Relaxed).max(1) as usize;
            if pending < 0 {
                let most = if whole { frames } else { step.min(frames) };
                let n = (pending.unsigned_abs() as usize).min(most);
                bytes.truncate((frames - n) * FRAME_BYTES);
                self.applied_frames -= n as i64;
                inner.last_removed = Some(Instant::now());
            } else {
                let most = if whole { MAX_ADDED_AT_ONCE } else { step };
                let n = (pending as usize).min(most);
                let last: [u8; FRAME_BYTES] = bytes[(frames - 1) * FRAME_BYTES..frames * FRAME_BYTES].try_into().unwrap();
                bytes.reserve(n * FRAME_BYTES);
                for _ in 0..n {
                    bytes.extend_from_slice(&last);
                }
                self.applied_frames += n as i64;
            }
        }
        self.delivered_frames += (bytes.len() / FRAME_BYTES) as u64;
        if let Some(c) = inner.current.as_mut().filter(|c| c.number == self.number) {
            c.applied_frames = self.applied_frames;
            c.delivered_frames = self.delivered_frames;
        }
    }

    /// The stream couldn't queue `frames` for this fetch.
    pub fn dropped(&self, frames: u64) {
        let mut inner = self.up.inner.lock().unwrap();
        if let Some(c) = inner.current.as_mut().filter(|c| c.number == self.number) {
            c.dropped_frames += frames;
        }
    }
}

/// Whether every sample of `bytes` is silence.
fn is_silent(bytes: &[u8]) -> bool {
    bytes.chunks_exact(2).all(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs() <= SILENT_LEVEL as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One packet of `frames` frames of sound (or silence at level 0).
    fn packet(frames: usize, level: i16) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * FRAME_BYTES);
        for i in 0..frames {
            let s = if level == 0 { 0 } else { level + (i % 7) as i16 };
            out.extend_from_slice(&s.to_le_bytes());
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn catch_up(step: u32) -> Arc<CatchUp> {
        CatchUp::new(Arc::new(AtomicU32::new(step)))
    }

    /// Frames out for `n` packets of 441 frames of sound.
    fn run(l: &mut Listener, n: usize) -> usize {
        (0..n)
            .map(|_| {
                let mut p = packet(441, 1000);
                l.edit(&mut p);
                p.len() / FRAME_BYTES
            })
            .sum()
    }

    #[test]
    fn removing_drops_a_few_frames_per_packet_until_done() {
        let c = catch_up(4);
        c.begin_session(-25);
        let mut l = c.listener();
        assert_eq!(c.ms_to_go(), -25);
        // 25 ms is 1102 frames: 276 packets at 4 frames each.
        let out = run(&mut l, 300);
        assert_eq!(out, 300 * 441 - 1102);
        assert_eq!(c.ms_to_go(), 0);
        // Done: the stream goes through untouched.
        let mut p = packet(441, 1000);
        let before = p.clone();
        l.edit(&mut p);
        assert_eq!(p, before);
    }

    #[test]
    fn adding_repeats_the_last_frame() {
        let c = catch_up(4);
        c.begin_session(100);
        let mut l = c.listener();
        let mut p = packet(441, 1000);
        let last = p[440 * 4..].to_vec();
        l.edit(&mut p);
        assert_eq!(p.len(), 445 * 4);
        for f in 441..445 {
            assert_eq!(&p[f * 4..(f + 1) * 4], &last[..]);
        }
        let out = run(&mut l, 2000);
        assert_eq!(out + 445, 2001 * 441 + 4410);
        assert_eq!(c.ms_to_go(), 0);
    }

    #[test]
    fn silence_takes_the_whole_change_at_once() {
        let c = catch_up(4);
        c.begin_session(-200);
        let mut l = c.listener();
        // 200 ms is 8820 frames: twenty 10 ms packets of silence go.
        for _ in 0..20 {
            let mut p = packet(441, 0);
            l.edit(&mut p);
            assert!(p.is_empty());
        }
        assert_eq!(c.ms_to_go(), 0);
        // Adding into silence: up to 100 ms per packet.
        c.set_target(100);
        let mut p = packet(441, 0);
        l.edit(&mut p);
        assert_eq!(p.len() / FRAME_BYTES, 441 + 4410);
    }

    #[test]
    fn at_once_skips_without_steps() {
        let c = catch_up(4);
        c.set_at_once(true);
        c.begin_session(-20);
        let mut l = c.listener();
        let mut p = packet(441, 1000);
        l.edit(&mut p);
        let mut q = packet(441, 1000);
        l.edit(&mut q);
        // 882 frames removed: both packets, whole.
        assert_eq!((p.len() / 4, q.len() / 4), (0, 0));
        let mut r = packet(441, 1000);
        l.edit(&mut r);
        assert_eq!(r.len() / 4, 441);
    }

    #[test]
    fn a_step_larger_than_the_packet_counts_what_it_dropped() {
        let c = catch_up(4096);
        c.begin_session(-100);
        let mut l = c.listener();
        let mut total = 0;
        for _ in 0..100 {
            let mut p = packet(88, 1000);
            l.edit(&mut p);
            total += p.len() / 4;
        }
        assert_eq!(total, 100 * 88 - 4410);
    }

    #[test]
    fn each_fetch_applies_the_whole_change_again() {
        let c = catch_up(4);
        c.begin_session(-25);
        let mut first = c.listener();
        // A short first fetch (the speaker probing), then the real one.
        run(&mut first, 10);
        let mut second = c.listener();
        assert_eq!(c.target_ms(), -25, "no back-off for a probe");
        assert_eq!(c.ms_to_go(), -25);
        assert_eq!(run(&mut second, 300), 300 * 441 - 1102);
        // The first one, still being served, no longer reports.
        run(&mut first, 10);
        assert_eq!(c.ms_to_go(), 0);
    }

    /// A speaker that runs dry soon after delay was removed cut out: that
    /// is reported, with what was removed, and nothing changes; the app
    /// never puts delay back by itself.
    #[test]
    fn a_speaker_that_runs_dry_is_a_cut_out_and_nothing_changes() {
        let cuts = Arc::new(Mutex::new(Vec::new()));
        let c = catch_up(4);
        let b = cuts.clone();
        c.on_cut_out(move |x| b.lock().unwrap().push(x));
        c.begin_session(-200);
        let mut l = c.listener();
        // Plays 3 s (removing as it goes), then the speaker fetches again.
        run(&mut l, 300);
        let mut again = c.listener();
        assert_eq!(c.target_ms(), -200, "nothing goes back");
        assert_eq!(*cuts.lock().unwrap(), vec![CutOut { removed_ms: 200 }]);
        // The new fetch applies the whole change again (8820 frames, 4 a
        // packet).
        run(&mut again, 2300);
        assert_eq!(c.ms_to_go(), 0);
        // The same for a refill on the same fetch.
        c.set_target(-30);
        run(&mut again, 300);
        c.speaker_rebuffered();
        assert_eq!(c.target_ms(), -30);
        assert_eq!(cuts.lock().unwrap().len(), 2);
        // Nothing removed: no cut-out of the app's making.
        c.set_target(0);
        run(&mut again, 300);
        let _third = c.listener();
        assert_eq!(cuts.lock().unwrap().len(), 2);
    }

    #[test]
    fn added_delay_is_never_backed_off() {
        let c = catch_up(4);
        c.begin_session(100);
        let mut l = c.listener();
        run(&mut l, 300);
        let _again = c.listener();
        assert_eq!(c.target_ms(), 100);
    }

    #[test]
    fn a_refill_applies_the_change_again_on_the_same_fetch() {
        let c = catch_up(4);
        c.begin_session(-25);
        let mut l = c.listener();
        run(&mut l, 300);
        assert_eq!(c.ms_to_go(), 0);
        c.restarted();
        assert_eq!(c.ms_to_go(), -25);
        assert_eq!(run(&mut l, 300), 300 * 441 - 1102);
        assert_eq!(c.target_ms(), -25, "a restart asked for is no reason to give delay back");
    }

    #[test]
    fn a_new_session_starts_from_nothing_applied() {
        let c = catch_up(4);
        c.begin_session(-25);
        let mut l = c.listener();
        run(&mut l, 300);
        c.begin_session(-50);
        assert_eq!(c.ms_to_go(), -50);
        // A fetch for the new session is its first: no back-off.
        let _l = c.listener();
        assert_eq!(c.target_ms(), -50);
    }

    #[test]
    fn drops_are_counted_for_the_newest_fetch() {
        let c = catch_up(4);
        c.begin_session(0);
        let first = c.listener();
        let second = c.listener();
        first.dropped(100);
        second.dropped(441);
        assert_eq!(c.dropped_frames(), 441);
    }
}
