//! Captured sound for the PC's own output, before any speaker processing.

use std::sync::{Arc, Mutex};

use crate::engine::place::numbers::{MAX_SAMPLE_MS, SHORT_SOUND_MS, SOUND_GAP_MS};
use crate::audio_source::LocalPlayback;
use crate::platform::{PassThrough, PcOutputId};
use crate::{WIRE_CHANNELS, WIRE_SAMPLE_RATE};

/// A short sound is replayed once the rules know it has ended. It lasts up
/// to [`SHORT_SOUND_MS`]; its end is known [`SOUND_GAP_MS`] and up to one
/// sample of the levels later; and it began up to one sample before it was
/// first heard. The capture holds that much.
const CAPTURE_MS: u64 = SHORT_SOUND_MS + SOUND_GAP_MS + 2 * MAX_SAMPLE_MS;
const CAPTURE_FRAMES: usize = (CAPTURE_MS * WIRE_SAMPLE_RATE as u64 / 1000) as usize;

#[derive(Clone)]
pub(crate) struct Tap(Arc<Mutex<Capture>>);

struct Capture {
    samples: Vec<i16>,
    next: usize,
    len: usize,
    pass: Option<(PcOutputId, Arc<dyn PassThrough>)>,
    playback: LocalPlayback,
}

impl Tap {
    pub(super) fn new() -> Self {
        Self::with_frames(CAPTURE_FRAMES)
    }

    pub(super) fn with_frames(frames: usize) -> Self {
        let samples = vec![0; frames * WIRE_CHANNELS as usize];
        Self(Arc::new(Mutex::new(Capture { samples, next: 0, len: 0, pass: None, playback: LocalPlayback::Any })))
    }

    #[cfg(test)]
    pub(super) fn block(&self, samples: &[i16]) { self.block_from(samples, &LocalPlayback::Any); }

    pub(crate) fn block_from(&self, samples: &[i16], playback: &LocalPlayback) {
        let mut c = self.0.lock().unwrap();
        if c.playback != *playback {
            c.len = 0;
            c.playback = playback.clone();
        }
        if matches!(playback, LocalPlayback::WindowsDefault | LocalPlayback::None) { return; }
        let capacity = c.samples.len();
        let keep = &samples[samples.len().saturating_sub(capacity)..];
        let next = c.next;
        let first = keep.len().min(capacity - next);
        c.samples[next..next + first].copy_from_slice(&keep[..first]);
        c.samples[..keep.len() - first].copy_from_slice(&keep[first..]);
        c.next = (next + keep.len()) % capacity;
        c.len = capacity.min(c.len + keep.len());
        // write returns immediately. Disabling the tap waits only for this
        // block, so none of its samples can land in a later replay.
        if let Some((_, pass)) = c.pass.as_ref().filter(|(id, _)| playback.allows(id)) {
            pass.write(samples, WIRE_SAMPLE_RATE, WIRE_CHANNELS);
        }
    }

    pub(super) fn pass(&self, to: Option<(PcOutputId, Arc<dyn PassThrough>)>) {
        self.0.lock().unwrap().pass = to;
    }

    /// Take the sound of the last `ms` milliseconds, in capture order and
    /// without the silent frames before and after it. The capture is empty
    /// afterwards, so that nothing is replayed twice. Allocation and
    /// trimming happen outside the lock shared with the audio thread.
    pub(super) fn take(&self, ms: u64, on: &PcOutputId) -> Vec<i16> {
        let channels = WIRE_CHANNELS as usize;
        let frames = usize::try_from(ms.saturating_mul(WIRE_SAMPLE_RATE as u64) / 1000).unwrap_or(usize::MAX);
        let wanted = frames.min(CAPTURE_FRAMES).saturating_mul(channels);
        let mut samples = Vec::with_capacity(wanted);
        {
            let mut c = self.0.lock().unwrap();
            let capacity = c.samples.len();
            let take = if c.playback.allows(on) { wanted.min(c.len) } else { 0 };
            // The newest sample is the one before `next`.
            let start = (c.next + capacity - take) % capacity;
            let first = take.min(capacity - start);
            samples.extend_from_slice(&c.samples[start..start + first]);
            samples.extend_from_slice(&c.samples[..take - first]);
            c.next = 0;
            c.len = 0;
        }
        let sounds = |frame: &[i16]| frame.iter().any(|s| *s != 0);
        let end = samples.chunks_exact(channels).rposition(sounds).map_or(0, |last| (last + 1) * channels);
        samples.truncate(end);
        let start = samples.chunks_exact(channels).position(sounds).map_or(0, |first| first * channels);
        samples.drain(..start);
        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long enough to reach back over any of these small captures.
    const ALL: u64 = 1_000;

    fn speakers() -> PcOutputId { PcOutputId::new("speakers") }

    #[test]
    fn the_sound_is_taken_in_order_without_the_silent_frames_around_it() {
        let tap = Tap::with_frames(3);
        assert!(tap.take(ALL, &speakers()).is_empty());
        tap.block(&[0, 0]);
        assert!(tap.take(ALL, &speakers()).is_empty());
        tap.block(&[1, 1, 0, 0, 0, 2]);
        tap.block(&[3, 4]);
        assert_eq!(tap.take(ALL, &speakers()), [0, 2, 3, 4]);
        // More than the capture holds: the latest frames are kept.
        tap.block(&[9, 9, 1, 2, 3, 4, 5, 6]);
        assert_eq!(tap.take(ALL, &speakers()), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn silence_after_the_sound_is_left_out_and_silence_inside_it_is_kept() {
        let tap = Tap::with_frames(5);
        tap.block(&[0, 0, 7, 7, 0, 0, 8, 8]);
        tap.block(&[0, 0]);
        assert_eq!(tap.take(ALL, &speakers()), [7, 7, 0, 0, 8, 8]);
    }

    #[test]
    fn what_was_taken_is_not_taken_again() {
        let tap = Tap::with_frames(3);
        tap.block(&[1, 2]);
        assert_eq!(tap.take(ALL, &speakers()), [1, 2]);
        assert!(tap.take(ALL, &speakers()).is_empty());
        tap.block(&[3, 4]);
        assert_eq!(tap.take(ALL, &speakers()), [3, 4]);
    }

    #[test]
    fn only_the_last_milliseconds_asked_for_are_taken() {
        let ms = |n: usize| vec![n as i16; n * WIRE_SAMPLE_RATE as usize / 1000 * WIRE_CHANNELS as usize];
        let tap = Tap::with_frames(WIRE_SAMPLE_RATE as usize / 10);
        // 30 ms of an older sound, then 20 ms of the one that just ended.
        tap.block(&ms(30));
        tap.block(&ms(20));
        assert_eq!(tap.take(20, &speakers()), ms(20));
        // The capture wrapped: 60 ms, then 70 ms, in a capture of 100 ms.
        tap.block(&ms(60));
        tap.block(&ms(70));
        assert_eq!(tap.take(70, &speakers()), ms(70));
    }
}
