//! Shared-mode rendering of interleaved PCM on one active PC output.

use std::sync::atomic::{fence, AtomicI16, AtomicU16, AtomicU32, AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::Arc;

/// One writer publishes complete frames. The render thread has its own
/// cursor, so a stalled endpoint can never hold up the writer.
struct Ring {
    samples: Vec<AtomicI16>,
    info: Vec<FrameInfo>,
    stride: usize,
    oldest: AtomicU64,
    end: AtomicU64,
}

#[derive(Default)]
struct FrameInfo {
    rate: AtomicU32,
    channels: AtomicU16,
    // End of this frame on the input timeline, in units of 2^-32 seconds.
    until: AtomicU64,
}

const HALF_SECOND: u64 = 1 << 31;

impl Ring {
    fn capacity_for(rate: u32, stride: usize) -> usize {
        // Bound storage even for malformed input. The full half second fits
        // up to 2 MHz with sixteen channels, including ordinary PC formats.
        (rate.div_ceil(2) as usize).min(1 << 20).min((16 << 20) / stride)
    }

    fn new(rate: u32, stride: usize) -> Option<Self> {
        if rate == 0 || stride == 0 {
            return None;
        }
        let capacity = Self::capacity_for(rate, stride);
        let mut samples = Vec::new();
        let mut info = Vec::new();
        samples.try_reserve_exact(capacity * stride).ok()?;
        info.try_reserve_exact(capacity).ok()?;
        samples.resize_with(capacity * stride, AtomicI16::default);
        info.resize_with(capacity, FrameInfo::default);
        Some(Self { samples, info, stride, oldest: 0.into(), end: 0.into() })
    }

    fn slot(&self, frame: u64) -> usize {
        (frame % self.info.len() as u64) as usize
    }

    // Grow on the writer thread. Both rings use the same frame numbers, so
    // a reader finishing a packet on the old ring can continue on the new one.
    fn grow(&self, rate: u32) -> Option<Self> {
        let new = Self::new(rate, self.stride)?;
        let oldest = self.oldest.load(Ordering::Relaxed);
        let end = self.end.load(Ordering::Relaxed);
        for frame in oldest..end {
            let from = self.slot(frame);
            let to = new.slot(frame);
            let old_info = &self.info[from];
            let new_info = &new.info[to];
            new_info.rate.store(old_info.rate.load(Ordering::Relaxed), Ordering::Relaxed);
            new_info.channels.store(old_info.channels.load(Ordering::Relaxed), Ordering::Relaxed);
            new_info.until.store(old_info.until.load(Ordering::Relaxed), Ordering::Relaxed);
            for ch in 0..self.stride {
                let value = self.samples[from * self.stride + ch].load(Ordering::Relaxed);
                new.samples[to * self.stride + ch].store(value, Ordering::Relaxed);
            }
        }
        new.oldest.store(oldest, Ordering::Relaxed);
        new.end.store(end, Ordering::Release);
        Some(new)
    }

    // The producer mutex serialises writers; the consumer never takes it.
    fn push(&self, input: &[i16], rate: u32, channels: u16) {
        let width = usize::from(channels);
        let count = input.len() / width;
        let before = self.end.load(Ordering::Relaxed);
        let start_time = if before == 0 { 0 } else { self.info[self.slot(before - 1)].until.load(Ordering::Relaxed) };
        let ticks = ((1u64 << 32) + u64::from(rate) / 2) / u64::from(rate);
        let end = before + count as u64;
        let end_time = start_time.wrapping_add((count as u64).wrapping_mul(ticks));
        let keep = count.min(self.info.len()).min(rate.div_ceil(2) as usize);
        let mut oldest = self.oldest.load(Ordering::Relaxed).max(end.saturating_sub(self.info.len() as u64));
        if keep < count {
            oldest = end - keep as u64;
        }
        // Discard old audio by elapsed time as well as slot count: half a
        // second may contain frames at several different sample rates.
        while oldest < before
            && end_time.wrapping_sub(self.info[self.slot(oldest)].until.load(Ordering::Relaxed)) >= HALF_SECOND
        {
            oldest += 1;
        }
        self.oldest.store(oldest, Ordering::Relaxed);
        // If a reader observes any overwritten sample, its matching acquire
        // fence also makes this new lower bound visible to its final check.
        fence(Ordering::Release);
        let skip = count - keep;
        let copy = width.min(self.stride);
        for (offset, frame) in input[skip * width..].chunks_exact(width).enumerate() {
            let index = skip + offset;
            let slot = self.slot(before + index as u64);
            let start = slot * self.stride;
            for (sample, value) in self.samples[start..start + copy].iter().zip(frame) {
                sample.store(*value, Ordering::Relaxed);
            }
            let info = &self.info[slot];
            info.rate.store(rate, Ordering::Relaxed);
            info.channels.store(copy as u16, Ordering::Relaxed);
            info.until.store(start_time.wrapping_add((index as u64 + 1).wrapping_mul(ticks)), Ordering::Relaxed);
        }
        self.end.store(end, Ordering::Release);
    }

    fn source(&self, frame: u64) -> (u32, usize) {
        let info = &self.info[self.slot(frame)];
        (info.rate.load(Ordering::Relaxed), usize::from(info.channels.load(Ordering::Relaxed)))
    }

    fn sample(&self, frame: u64, channel: usize) -> f64 {
        let index = self.slot(frame) * self.stride + channel;
        f64::from(self.samples[index].load(Ordering::Relaxed)) / 32768.0
    }

    fn mapped(&self, frame: u64, channels: usize, output_channels: usize, channel: usize, mask: u32) -> f64 {
        if output_channels == 1 && channels >= 2 {
            return (self.sample(frame, 0) + self.sample(frame, 1)) * 0.5;
        }
        if channels <= 2 && mask != 0 && output_channels > 1 {
            // WAVEFORMATEXTENSIBLE orders channels by ascending speaker bit.
            // Only the first two speaker bits are front left and front right.
            if channel >= (mask & 3).count_ones() as usize {
                return 0.0;
            }
            let right = channel == 1 || mask & 1 == 0;
            return self.sample(frame, usize::from(right && channels == 2));
        }
        if channels == 1 && channel < 2 {
            self.sample(frame, 0)
        } else if channel < channels {
            self.sample(frame, channel)
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy)]
enum Sample {
    Float32,
    Float64,
    // PCM valid bits occupy the high bits of the sample container.
    Integer { bytes: usize, bits: u16 },
}

impl Sample {
    fn bytes(self) -> usize {
        match self {
            Self::Float32 => 4,
            Self::Float64 => 8,
            Self::Integer { bytes, .. } => bytes,
        }
    }

    fn write(self, dst: &mut [u8], value: f64) {
        match self {
            Self::Float32 => dst.copy_from_slice(&(value as f32).to_le_bytes()),
            Self::Float64 => dst.copy_from_slice(&value.to_le_bytes()),
            Self::Integer { bytes, bits } => {
                let scale = 1i64 << (bits - 1);
                let signed = (value * scale as f64).round().clamp(-scale as f64, (scale - 1) as f64) as i64;
                let sample = if bytes == 1 { signed + scale } else { signed };
                let aligned = sample << (bytes * 8 - usize::from(bits));
                dst.copy_from_slice(&aligned.to_le_bytes()[..bytes]);
            }
        }
    }
}

struct Format {
    rate: u32,
    channels: usize,
    mask: u32,
    sample: Sample,
}

impl Format {
    fn stride(&self) -> usize {
        self.channels * self.sample.bytes()
    }

    fn silence(&self, bytes: &mut [u8]) {
        bytes.fill(if matches!(self.sample, Sample::Integer { bytes: 1, .. }) { 128 } else { 0 });
    }
}

/// The next position in input frames. Its fraction has the output sample
/// rate as denominator, so repeated render calls accumulate no rounding error.
#[derive(Default)]
struct Cursor {
    frame: u64,
    fraction: u64,
    skip: u64,
    source: (u32, usize),
}

impl Cursor {
    fn render(&mut self, ring: &Ring, output: &Format, dst: &mut [u8], draining: bool) -> usize {
        let end = ring.end.load(Ordering::Acquire);
        let oldest = ring.oldest.load(Ordering::Acquire);
        if self.frame < oldest {
            *self = Self { frame: oldest, ..Self::default() };
        }
        let first = self.frame;
        let mut rendered = 0;
        for frame in dst.chunks_exact_mut(output.stride()) {
            while self.frame < end {
                let source = ring.source(self.frame);
                if self.source != source {
                    self.source = source;
                    self.fraction = 0;
                    self.skip = 0;
                }
                if self.skip == 0 {
                    break;
                }
                self.frame += 1;
                self.skip -= 1;
            }
            if self.frame >= end {
                break;
            }
            let (rate, channels) = self.source;
            if self.frame + 1 == end && self.fraction != 0 && !draining {
                break;
            }
            // At a format boundary the final sample lasts for the remainder
            // of its input period. Start the next format at its first sample.
            let next = self.frame + 1;
            let interpolate = self.fraction != 0 && next < end && ring.source(next) == self.source;
            let t = self.fraction as f64 / f64::from(output.rate);
            for (ch, dst) in frame.chunks_exact_mut(output.sample.bytes()).enumerate() {
                let a = ring.mapped(self.frame, channels, output.channels, ch, output.mask);
                let value = if interpolate {
                    let b = ring.mapped(next, channels, output.channels, ch, output.mask);
                    a + (b - a) * t
                } else {
                    a
                };
                output.sample.write(dst, value);
            }
            self.fraction += u64::from(rate);
            self.skip = self.fraction / u64::from(output.rate);
            self.fraction %= u64::from(output.rate);
            rendered += 1;
        }
        // A writer may have lapped us during conversion. Discard that whole
        // packet, including any frame whose channels came from different writes.
        fence(Ordering::Acquire);
        if first < ring.oldest.load(Ordering::Relaxed) {
            *self = Self { frame: ring.oldest.load(Ordering::Acquire), ..Self::default() };
            return 0;
        }
        rendered
    }
}

#[cfg(windows)]
pub(super) use native::WindowsPass;

#[cfg(windows)]
mod native {
    use super::*;
    use crate::platform::win::{endpoint_info, is_ours, Apartment};
    use crate::platform::{Outputs, PassThrough, PcOutputId};
    use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
    use std::sync::atomic::AtomicU8;
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::core::{w, Interface, Owned, HSTRING};
    use windows::Win32::Foundation::{HANDLE, WAIT_FAILED};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
    use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
    use windows::Win32::System::Threading::{
        AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject,
    };

    const RUN: u8 = 0;
    const DRAIN: u8 = 1;
    const STOP: u8 = 2;
    const CLOSE_LIMIT: Duration = Duration::from_millis(3500);

    struct Feed {
        stride: usize,
        mode: Arc<AtomicU8>,
        ring: Option<Arc<Ring>>,
        updates: Sender<Arc<Ring>>,
        pending: Receiver<Arc<Ring>>,
    }

    impl Feed {
        fn write(&mut self, frames: &[i16], rate: u32, channels: u16) {
            if self.mode.load(Ordering::Acquire) != RUN
                || rate == 0
                || channels == 0
                || frames.len() < usize::from(channels)
            {
                return;
            }
            let new = match &self.ring {
                Some(ring) if Ring::capacity_for(rate, self.stride) <= ring.info.len() => {
                    ring.push(frames, rate, channels);
                    return;
                }
                Some(ring) => ring.grow(rate),
                None => Ring::new(rate, self.stride),
            };
            let Some(ring) = new.map(Arc::new) else { return };
            ring.push(frames, rate, channels);
            // One writer publishes replacement rings. If the renderer hasn't
            // collected the previous one, the larger ring carries its history.
            if let Err(TrySendError::Full(_)) = self.updates.try_send(ring.clone()) {
                let _ = self.pending.try_recv();
                let _ = self.updates.try_send(ring.clone());
            }
            self.ring = Some(ring);
        }
    }

    #[derive(Clone)]
    struct Stream {
        mode: Arc<AtomicU8>,
        done: Receiver<()>,
    }

    impl Stream {
        fn finish(&self, deadline: Instant) {
            let _ = self.done.recv_deadline(deadline);
            self.mode.store(STOP, Ordering::Release);
        }
    }

    #[derive(Default)]
    struct State {
        generation: u64,
        stream: Option<Stream>,
        feed: Option<Feed>,
    }

    pub(in crate::platform::win) struct WindowsPass {
        outputs: Arc<dyn Outputs>,
        state: Mutex<State>,
    }

    impl WindowsPass {
        pub(in crate::platform::win) fn new(outputs: Arc<dyn Outputs>) -> Self {
            Self { outputs, state: Mutex::default() }
        }
    }

    impl PassThrough for WindowsPass {
        fn open(&self, output: &PcOutputId) -> Result<(), String> {
            if self.outputs.our_output().as_ref() == Some(output) {
                return Err("This app cannot play back on its own device.".into());
            }
            // Opening an endpoint may wait on a driver. A concurrent close or
            // a newer open can supersede it without waiting for that driver.
            let generation = {
                let mut state = self.state.lock().unwrap();
                state.generation += 1;
                state.generation
            };
            let (ready_tx, ready) = bounded(1);
            let (updates, pending) = bounded(1);
            let (done_tx, done) = bounded(1);
            let mode = Arc::new(AtomicU8::new(RUN));
            let worker_mode = mode.clone();
            let rx = pending.clone();
            let output = output.clone();
            thread::Builder::new()
                .name("stream-to-speaker-pass".into())
                .spawn(move || {
                    let result = run(&output, &worker_mode, rx, &ready_tx);
                    worker_mode.store(STOP, Ordering::Release);
                    if let Err(error) = result {
                        log::debug!("PC audio output ended: {error}");
                        let _ = ready_tx.try_send(Err(error));
                    }
                    let _ = done_tx.send(());
                })
                .map_err(|_| "The audio render thread could not start.")?;
            let stride = match ready.recv_timeout(Duration::from_secs(2)) {
                Ok(result) => result?,
                Err(_) => {
                    mode.store(STOP, Ordering::Release);
                    return Err("The output did not start in time.".into());
                }
            };
            let old = {
                let mut state = self.state.lock().unwrap();
                if generation != state.generation {
                    mode.store(STOP, Ordering::Release);
                    return Err("The requested output has changed.".into());
                }
                if let Some(old) = &state.stream {
                    old.mode.store(STOP, Ordering::Release);
                }
                state.feed = Some(Feed { stride, mode: mode.clone(), ring: None, updates, pending });
                state.stream.replace(Stream { mode, done })
            };
            if let Some(old) = old {
                old.finish(Instant::now() + CLOSE_LIMIT);
            }
            Ok(())
        }

        fn write(&self, frames: &[i16], rate: u32, channels: u16) {
            if let Ok(mut state) = self.state.try_lock() {
                if let Some(feed) = state.feed.as_mut() {
                    feed.write(frames, rate, channels);
                }
            }
        }

        fn close(&self) {
            let deadline = Instant::now() + CLOSE_LIMIT;
            let (stream, generation) = {
                let mut state = self.state.lock().unwrap();
                state.generation += 1;
                state.feed.take();
                let Some(stream) = &state.stream else { return };
                let _ = stream.mode.compare_exchange(RUN, DRAIN, Ordering::AcqRel, Ordering::Acquire);
                (stream.clone(), state.generation)
            };
            stream.finish(deadline);
            let mut state = self.state.lock().unwrap();
            if state.generation == generation {
                state.stream.take();
            }
        }
    }

    impl Drop for WindowsPass {
        fn drop(&mut self) {
            if let Some(stream) = self.state.get_mut().unwrap().stream.take() {
                stream.mode.store(STOP, Ordering::Release);
            }
        }
    }

    struct AudioPriority(HANDLE);
    impl Drop for AudioPriority {
        fn drop(&mut self) {
            // SAFETY: this worker registered its own MMCSS handle.
            let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
        }
    }

    struct Mix(*mut WAVEFORMATEX);
    impl Drop for Mix {
        fn drop(&mut self) {
            // SAFETY: GetMixFormat transfers one task-allocated format to us.
            unsafe { CoTaskMemFree(Some(self.0.cast())) };
        }
    }

    impl Mix {
        fn format(&self) -> Option<Format> {
            // SAFETY: GetMixFormat provides a WAVEFORMATEX and cbSize extra
            // bytes. Read the packed extension only after checking its extent.
            let (base, tag, valid, mask) = unsafe {
                let base = *self.0.as_ref()?;
                let tag = u32::from(base.wFormatTag);
                if tag == WAVE_FORMAT_EXTENSIBLE {
                    if usize::from(base.cbSize) < size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>() {
                        return None;
                    }
                    let ext = self.0.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned();
                    let subtype = ext.SubFormat;
                    let tag = if subtype == KSDATAFORMAT_SUBTYPE_PCM {
                        WAVE_FORMAT_PCM
                    } else if subtype == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                        WAVE_FORMAT_IEEE_FLOAT
                    } else {
                        return None;
                    };
                    (base, tag, ext.Samples.wValidBitsPerSample, ext.dwChannelMask)
                } else {
                    (base, tag, base.wBitsPerSample, 0)
                }
            };
            let bits = base.wBitsPerSample;
            let valid = if valid == 0 { bits } else { valid };
            let sample = match (tag, bits) {
                (WAVE_FORMAT_IEEE_FLOAT, 32) => Sample::Float32,
                (WAVE_FORMAT_IEEE_FLOAT, 64) => Sample::Float64,
                (WAVE_FORMAT_PCM, 8 | 16 | 24 | 32) if valid <= bits => {
                    Sample::Integer { bytes: usize::from(bits / 8), bits: valid }
                }
                _ => return None,
            };
            let format = Format { rate: base.nSamplesPerSec, channels: usize::from(base.nChannels), mask, sample };
            (format.rate > 0 && format.channels > 0 && format.stride() == usize::from(base.nBlockAlign))
                .then_some(format)
        }
    }

    fn run(
        output: &PcOutputId,
        mode: &AtomicU8,
        rings: Receiver<Arc<Ring>>,
        ready: &Sender<Result<usize, String>>,
    ) -> Result<(), String> {
        let _apartment = Apartment::enter().map_err(|_| "Windows audio could not start.")?;
        // SAFETY: MMCSS reads a static task name and fills a local task index.
        let _priority = unsafe { AvSetMmThreadCharacteristicsW(w!("Audio"), &mut 0) }.ok().map(AudioPriority);
        let inactive = "That output is not active.";
        let failed = "Windows could not start playback on that output.";
        if output.0.contains('\0') {
            return Err(inactive.into());
        }
        // SAFETY: no security descriptor or name is supplied. Owned closes the
        // auto-reset event after the audio client has stopped using its handle.
        let event = unsafe { Owned::new(CreateEventW(None, false, false, None).map_err(|_| failed)?) };
        // SAFETY: this MTA owns every interface. Mix owns the format through
        // Initialize. The shared event stream uses the engine's period and
        // buffer size. Its first reservation is released as silence before Start.
        let (client, render, format, capacity) = unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|_| failed)?;
            let device = enumerator.GetDevice(&HSTRING::from(output.0.as_ref())).map_err(|_| inactive)?;
            if device.GetState().map_err(|_| inactive)? != DEVICE_STATE_ACTIVE
                || device.cast::<IMMEndpoint>().and_then(|e| e.GetDataFlow()).ok() != Some(eRender)
            {
                return Err(inactive.into());
            }
            if is_ours(&endpoint_info(output, &device).ok_or("Windows could not identify that output.")?) {
                return Err("This app cannot play back on its own device.".into());
            }
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|_| failed)?;
            let mix = Mix(client.GetMixFormat().map_err(|_| failed)?);
            let format = mix.format().ok_or("Windows reported an unsupported output format.")?;
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, mix.0, None)
                .map_err(|_| failed)?;
            client.SetEventHandle(*event).map_err(|_| failed)?;
            let capacity = client.GetBufferSize().map_err(|_| failed)?;
            let render: IAudioRenderClient = client.GetService().map_err(|_| failed)?;
            if let Ok(session) = client.GetService::<IAudioSessionControl>() {
                let _ = session.SetDisplayName(&HSTRING::from(crate::PRODUCT_NAME), std::ptr::null());
            }
            if capacity == 0 {
                return Err(failed.into());
            }
            let initial = capacity.min(format.rate.div_ceil(50));
            render.GetBuffer(initial).map_err(|_| failed)?;
            render.ReleaseBuffer(initial, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).map_err(|_| failed)?;
            client.Start().map_err(|_| failed)?;
            (client, render, format, capacity)
        };
        let result = if ready.send(Ok(format.channels.max(2))).is_ok() {
            pump(&client, &render, &format, capacity, *event, mode, rings)
        } else {
            Ok(())
        };
        // SAFETY: Start succeeded on this client and every acquired render
        // buffer has been released before pump returns.
        let _ = unsafe { client.Stop() };
        result.map_err(|e| e.to_string())
    }

    fn pump(
        client: &IAudioClient,
        render: &IAudioRenderClient,
        format: &Format,
        capacity: u32,
        event: HANDLE,
        mode: &AtomicU8,
        rings: Receiver<Arc<Ring>>,
    ) -> windows::core::Result<()> {
        let mut ring = None;
        let mut cursor = Cursor::default();
        let target = capacity.min(format.rate.div_ceil(50)); // keep at most 20 ms in WASAPI
        let mut packet = vec![0; target as usize * format.stride()];
        loop {
            let mode = mode.load(Ordering::Acquire);
            if mode == STOP {
                break;
            }
            if let Ok(new) = rings.try_recv() {
                ring = Some(new);
            }
            let draining = mode == DRAIN;
            // SAFETY: the client and render service belong to this thread.
            // GetBuffer reserves exactly count * block-align writable bytes.
            // Each successful reservation has one ReleaseBuffer on this thread.
            unsafe {
                let padding = client.GetCurrentPadding()?;
                let frames = target.saturating_sub(padding);
                let empty = ring.as_ref().is_none_or(|r| cursor.frame >= r.end.load(Ordering::Acquire));
                if draining && empty {
                    if padding == 0 {
                        break;
                    }
                } else if frames > 0 {
                    let buf = &mut packet[..frames as usize * format.stride()];
                    let used = ring.as_ref().map_or(0, |r| cursor.render(r, format, buf, draining));
                    let count = if draining { used as u32 } else { frames };
                    if count != 0 {
                        let len = count as usize * format.stride();
                        format.silence(&mut buf[used * format.stride()..len]);
                        let ptr = render.GetBuffer(count)?;
                        std::ptr::copy_nonoverlapping(buf.as_ptr(), ptr, len);
                        render.ReleaseBuffer(count, 0)?;
                    }
                }
            }
            // SAFETY: the stream's auto-reset event outlives this pump. The
            // timeout also checks for shutdown or removal if its driver goes silent.
            if unsafe { WaitForSingleObject(event, 40) } == WAIT_FAILED {
                return Err(windows::core::Error::from_win32());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::platform::fake::{FakePlatform, OURS};

        #[test]
        fn refuses_the_apps_own_id() {
            let pass = WindowsPass::new(FakePlatform::new().platform().outputs);
            assert_eq!(pass.open(&PcOutputId::new(OURS)).unwrap_err(), "This app cannot play back on its own device.");
        }

        #[test]
        fn plays_a_quiet_second_and_closes() {
            let p = crate::platform::win::platform();
            if let Some(ours) = p.outputs.our_output() {
                assert!(p.pass.open(&ours).is_err());
            }
            let Some(out) = p.outputs.list().into_iter().find(|o| !is_ours(o)) else {
                eprintln!("No active PC render endpoint is available.");
                return;
            };
            p.pass.open(&out.id).unwrap();
            let tone: Vec<i16> = (0..44_100)
                .map(|i| (32.0 * (i as f32 * std::f32::consts::TAU * 440.0 / 44_100.0).sin()) as i16)
                .collect();
            for packet in tone.chunks_exact(441) {
                p.pass.write(packet, 44_100, 1);
                thread::sleep(Duration::from_millis(10));
            }
            let at = Instant::now();
            p.pass.close();
            assert!(at.elapsed() <= CLOSE_LIMIT);
            p.pass.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floats(ring: &Ring, cursor: &mut Cursor, rate: u32, channels: usize, count: usize, draining: bool) -> Vec<f32> {
        let format = Format { rate, channels, mask: 0, sample: Sample::Float32 };
        let mut dst = vec![0; count * format.stride()];
        let n = cursor.render(ring, &format, &mut dst, draining);
        dst[..n * format.stride()].chunks_exact(4).map(|s| f32::from_le_bytes(s.try_into().unwrap())).collect()
    }

    #[test]
    fn wrap_and_overflow_keep_the_latest_complete_frames() {
        let ring = Ring::new(8, 2).unwrap(); // four stereo frames
        let mut cursor = Cursor::default();
        ring.push(&[1, -1, 2, -2, 3, -3], 8, 2);
        let left = |samples: Vec<f32>| {
            samples
                .chunks_exact(2)
                .map(|s| {
                    assert_eq!(s[0], -s[1]);
                    (s[0] * 32768.0) as i16
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(left(floats(&ring, &mut cursor, 8, 2, 2, false)), [1, 2]);
        ring.push(&[4, -4, 5, -5], 8, 2);
        assert_eq!(left(floats(&ring, &mut cursor, 8, 2, 8, false)), [3, 4, 5]);
        ring.push(&[6, -6, 7, -7, 8, -8, 9, -9, 10, -10, 11, -11, 123], 8, 2);
        assert_eq!(left(floats(&ring, &mut cursor, 8, 2, 8, true)), [8, 9, 10, 11]);
    }

    #[test]
    fn a_writer_lapping_the_reader_cannot_tear_a_stereo_frame() {
        let ring = Ring::new(128, 2).unwrap();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for i in 1..20_000i16 {
                    ring.push(&[i, -i], 128, 2);
                }
            });
            let mut cursor = Cursor::default();
            loop {
                let done = writer.is_finished();
                let packet = floats(&ring, &mut cursor, 128, 2, 64, false);
                assert!(packet.chunks_exact(2).all(|s| s[0] == -s[1]));
                if done {
                    break;
                }
                std::thread::yield_now();
            }
        });
    }

    #[test]
    fn mono_stereo_surround_and_normalised_float() {
        let ring = Ring::new(44_100, 1).unwrap();
        ring.push(&[i16::MIN, 16384, i16::MAX], 44_100, 1);
        assert_eq!(
            floats(&ring, &mut Cursor::default(), 44_100, 2, 3, false),
            [-1.0, -1.0, 0.5, 0.5, 32767.0 / 32768.0, 32767.0 / 32768.0]
        );
        let ring = Ring::new(44_100, 2).unwrap();
        ring.push(&[8192, 16384], 44_100, 2);
        assert_eq!(floats(&ring, &mut Cursor::default(), 44_100, 1, 1, false), [0.375]);
        assert_eq!(floats(&ring, &mut Cursor::default(), 44_100, 6, 1, false), [0.25, 0.5, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(ring.mapped(0, 2, 6, 0, 0x3f), 0.25);
        assert_eq!(ring.mapped(0, 2, 6, 2, 0x3f), 0.0);
    }

    #[test]
    fn interpolation_crosses_writes_and_drains_the_last_frame() {
        let ring = Ring::new(8, 1).unwrap();
        let mut cursor = Cursor::default();
        ring.push(&[0, 8192], 8, 1);
        let mut out = floats(&ring, &mut cursor, 16, 1, 10, false);
        assert_eq!(out, [0.0, 0.125, 0.25]);
        ring.push(&[16384, 24576], 8, 1);
        out.extend(floats(&ring, &mut cursor, 16, 1, 10, true));
        assert_eq!(out, [0.0, 0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.75]);
        assert_eq!(floats(&ring, &mut Cursor::default(), 4, 1, 10, true), [0.0, 0.5]);
    }

    #[test]
    fn queued_audio_survives_a_change_of_input_format() {
        let ring = Ring::new(8, 2).unwrap();
        ring.push(&[0, 8192], 8, 1);
        let mut cursor = Cursor::default();
        assert_eq!(floats(&ring, &mut cursor, 16, 2, 1, false), [0.0, 0.0]);
        let ring = ring.grow(16).unwrap();
        ring.push(&[-16384, 16384, 8192, -8192], 16, 2);
        assert_eq!(
            floats(&ring, &mut cursor, 16, 2, 8, true),
            [0.125, 0.125, 0.25, 0.25, 0.25, 0.25, -0.5, 0.5, 0.25, -0.25]
        );
        ring.push(&[2048; 8], 16, 1);
        assert_eq!(floats(&ring, &mut Cursor::default(), 16, 1, 20, true), [0.0625; 8]);
    }

    #[test]
    fn slow_input_drops_old_frames_by_time_even_when_slots_remain() {
        let ring = Ring::new(16, 1).unwrap();
        ring.push(&[8192; 4], 16, 1);
        ring.push(&[16384; 3], 8, 1);
        assert_eq!(floats(&ring, &mut Cursor::default(), 16, 1, 16, true), [0.25, 0.25, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5]);
    }

    #[test]
    fn rational_resampling_preserves_duration() {
        for (input, output) in [(44_100, 48_000), (48_000, 44_100)] {
            let ring = Ring::new(input, 1).unwrap();
            let mut cursor = Cursor::default();
            let mut n = 0;
            for _ in 0..100 {
                ring.push(&vec![1234; input as usize / 100], input, 1);
                n += floats(&ring, &mut cursor, output, 1, 512, false).len();
            }
            n += floats(&ring, &mut cursor, output, 1, 512, true).len();
            assert_eq!(n, output as usize);
        }
    }

    #[test]
    fn packed_pcm_valid_bits_and_silence() {
        for (sample, value, expected) in [
            (Sample::Integer { bytes: 1, bits: 8 }, 0.0, vec![128]),
            (Sample::Integer { bytes: 2, bits: 16 }, -1.0, vec![0, 128]),
            (Sample::Integer { bytes: 3, bits: 24 }, 0.5, vec![0, 0, 64]),
            (Sample::Integer { bytes: 4, bits: 20 }, 1.0, vec![0, 240, 255, 127]),
            (Sample::Float64, 0.125, 0.125f64.to_le_bytes().to_vec()),
        ] {
            let mut dst = vec![0; sample.bytes()];
            sample.write(&mut dst, value);
            assert_eq!(dst, expected);
        }
        let unsigned = Format { rate: 8000, channels: 1, mask: 0, sample: Sample::Integer { bytes: 1, bits: 8 } };
        let mut dst = [1; 8];
        unsigned.silence(&mut dst);
        assert_eq!(dst, [128; 8]);
    }
}
