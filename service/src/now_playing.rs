//! Reads the OS "now playing" (title / artist / album / artwork /
//! position / play state) for the opt-in metadata-forwarding feature, and
//! decides what has to be re-sent to a speaker when it changes.
//!
//! On Windows this comes from the **System Media Transport Controls**
//! (`Windows.Media.Control`) — the same source that feeds the volume-key
//! media overlay. Apps that play media (Spotify, browsers, media players)
//! each register a session there. Everything is best-effort: a WinRT
//! hiccup, no session, or an unsupported OS just yields `None`, never an
//! error, and never touches the audio path.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the media session reports about playback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
    #[default]
    Other,
}

/// Cover art as encoded image bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artwork {
    pub bytes: Arc<Vec<u8>>,
    /// `image/jpeg` or `image/png` (sniffed from the bytes).
    pub mime: &'static str,
}

/// Images larger than this are re-encoded (JPEG, ≤ [`ARTWORK_MAX_DIM`]
/// px) before sending, or dropped if that isn't possible. Receivers show
/// a thumbnail; a multi-megabyte PNG would just hold the RTSP connection.
pub const ARTWORK_MAX_BYTES: usize = 512 * 1024;
/// Longest edge of re-encoded artwork (OwnTone sends ≤ 600×600).
pub const ARTWORK_MAX_DIM: u32 = 600;

impl Artwork {
    /// Wrap image bytes, or `None` if they aren't a JPEG/PNG we can label.
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        let mime = sniff_image_mime(&bytes)?;
        Some(Self { bytes: Arc::new(bytes), mime })
    }

    /// Cheap content fingerprint (FNV-1a) for change detection.
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.bytes.iter() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^ (self.bytes.len() as u64)
    }
}

/// `image/jpeg` / `image/png` from the magic bytes; anything else `None`.
pub fn sniff_image_mime(b: &[u8]) -> Option<&'static str> {
    if b.len() >= 3 && b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF {
        Some("image/jpeg")
    } else if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else {
        None
    }
}

/// Track position as reported by the media session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeline {
    /// Position at `captured`, ms from the start of the track.
    pub position_ms: u64,
    /// Track length in ms (> 0 — tracks without a length have no timeline).
    pub duration_ms: u64,
    /// When `position_ms` was true.
    pub captured: Instant,
}

/// A snapshot of what the OS reports as currently playing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub artwork: Option<Artwork>,
    pub timeline: Option<Timeline>,
    pub state: PlaybackState,
}

impl NowPlaying {
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.artist.is_empty() && self.album.is_empty()
    }

    /// Position extrapolated to `now` (it only advances while playing),
    /// clamped to the track length.
    pub fn position_ms_at(&self, now: Instant) -> Option<u64> {
        let tl = self.timeline?;
        let mut pos = tl.position_ms;
        if self.state == PlaybackState::Playing {
            pos += now.saturating_duration_since(tl.captured).as_millis() as u64;
        }
        Some(pos.min(tl.duration_ms))
    }

    /// One-line title for receivers that only take a single string (ICY
    /// `StreamTitle`): "Artist - Title", or whichever half exists.
    pub fn display_line(&self) -> String {
        match (self.artist.is_empty(), self.title.is_empty()) {
            (false, false) => format!("{} - {}", self.artist, self.title),
            (true, false) => self.title.clone(),
            (false, true) => self.artist.clone(),
            (true, true) => self.album.clone(),
        }
    }

    fn text_key(&self) -> (String, String, String) {
        (self.title.clone(), self.artist.clone(), self.album.clone())
    }
}

// ---------------------------------------------------------------------------
// Session choice
// ---------------------------------------------------------------------------

/// One media session as seen when choosing which one to forward.
#[derive(Clone, Debug)]
pub struct SessionCandidate {
    /// The app's id (`SourceAppUserModelId`), e.g. `Spotify.exe`,
    /// `Chrome`, `SpotifyAB.SpotifyMusic_…!Spotify`.
    pub app_id: String,
    pub state: PlaybackState,
    /// Windows' own "current" session (the one the media overlay shows).
    pub is_current: bool,
}

/// Pick the session to forward. A session that is actually playing beats
/// a paused one, even if Windows still calls the paused one "current".
/// Among several playing sessions, one whose app has live audio on our
/// device (`routed_apps`: executable names, lower-case, no `.exe`) wins,
/// then Windows' current one, then the first. With nothing playing, the
/// current session (e.g. paused) is used so its state is still known.
pub fn pick_session(cands: &[SessionCandidate], routed_apps: &[String]) -> Option<usize> {
    let playing: Vec<usize> = (0..cands.len())
        .filter(|&i| cands[i].state == PlaybackState::Playing)
        .collect();
    if !playing.is_empty() {
        let routed: Vec<usize> = playing
            .iter()
            .copied()
            .filter(|&i| app_matches_any(&cands[i].app_id, routed_apps))
            .collect();
        let pool = if routed.is_empty() { &playing } else { &routed };
        return pool
            .iter()
            .copied()
            .find(|&i| cands[i].is_current)
            .or_else(|| pool.first().copied());
    }
    (0..cands.len())
        .find(|&i| cands[i].is_current)
        .or_else(|| (0..cands.len()).find(|&i| cands[i].state == PlaybackState::Paused))
}

/// Whether a media-session app id belongs to one of the executables
/// (lower-case stems, e.g. `spotify`, `chrome`, `msedge`). Win32 apps
/// register as `Name.exe` or a bare name; packaged apps as
/// `Publisher.Name_hash!App` — matched on the stem appearing in the id.
fn app_matches_any(app_id: &str, exe_stems: &[String]) -> bool {
    let id = app_id.to_ascii_lowercase();
    let id = id.strip_suffix(".exe").unwrap_or(&id);
    exe_stems
        .iter()
        .filter(|s| s.len() >= 3)
        .any(|stem| id == stem || id.contains(stem.as_str()))
}

/// Lower-case executable stem from a full image path
/// (`C:\…\Spotify.exe` → `spotify`).
pub fn exe_stem(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

// ---------------------------------------------------------------------------
// What to (re)send
// ---------------------------------------------------------------------------

/// Which metadata parts to send now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub text: bool,
    pub artwork: bool,
    pub progress: bool,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        !(self.text || self.artwork || self.progress)
    }

    /// Drop the parts a receiver doesn't take.
    pub fn masked(self, text: bool, artwork: bool, progress: bool) -> Plan {
        Plan {
            text: self.text && text,
            artwork: self.artwork && artwork,
            progress: self.progress && progress,
        }
    }
}

/// A position that drifts this far from what we last told the receiver
/// (seek, resume after pause, app correcting itself) is re-sent.
const PROGRESS_DRIFT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
struct ProgressMark {
    at: Instant,
    position_ms: u64,
    duration_ms: u64,
    playing: bool,
}

/// What one speaker has been sent so far; [`Sent::plan`] diffs a fresh
/// snapshot against it so only changes go out.
#[derive(Clone, Debug, Default)]
pub struct Sent {
    text: Option<(String, String, String)>,
    artwork: Option<u64>,
    progress: Option<ProgressMark>,
}

impl Sent {
    pub fn plan(&self, np: &NowPlaying, now: Instant) -> Plan {
        if np.is_empty() {
            return Plan::default();
        }
        let text = self.text.as_ref() != Some(&np.text_key());
        let art_fp = np.artwork.as_ref().map(Artwork::fingerprint);
        // A new track re-sends its art even when it's the same image (same
        // album): receivers tie artwork to the item it arrived with.
        let artwork = art_fp.is_some() && (text || art_fp != self.artwork);
        let progress = match (np.timeline, np.position_ms_at(now)) {
            (Some(tl), Some(pos)) => {
                let playing = np.state == PlaybackState::Playing;
                match self.progress {
                    None => true,
                    Some(_) if text => true,
                    Some(m) => {
                        let expected = if m.playing {
                            m.position_ms + now.saturating_duration_since(m.at).as_millis() as u64
                        } else {
                            m.position_ms
                        };
                        let drift = expected.abs_diff(pos);
                        m.duration_ms != tl.duration_ms
                            || (playing && !m.playing)
                            || (playing && drift > PROGRESS_DRIFT.as_millis() as u64)
                    }
                }
            }
            _ => false,
        };
        Plan { text, artwork, progress }
    }

    /// Record that `plan`'s parts of `np` reached the receiver.
    pub fn record(&mut self, np: &NowPlaying, plan: Plan, now: Instant) {
        if plan.text {
            self.text = Some(np.text_key());
            // Artwork / position belong to the previous track now.
            self.artwork = None;
            self.progress = None;
        }
        if plan.artwork {
            self.artwork = np.artwork.as_ref().map(Artwork::fingerprint);
        }
        if plan.progress {
            if let (Some(tl), Some(pos)) = (np.timeline, np.position_ms_at(now)) {
                self.progress = Some(ProgressMark {
                    at: now,
                    position_ms: pos,
                    duration_ms: tl.duration_ms,
                    playing: np.state == PlaybackState::Playing,
                });
            }
        } else if let Some(m) = self.progress.as_mut() {
            // Paused → keep the mark frozen where it is, so a resume (or a
            // seek while paused) is noticed as a change.
            if m.playing && np.state != PlaybackState::Playing {
                if let Some(pos) = np.position_ms_at(now) {
                    *m = ProgressMark { at: now, position_ms: pos, playing: false, ..*m };
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The OS source
// ---------------------------------------------------------------------------

/// Watches the OS media sessions. `wait` returns early when Windows
/// reports a change (track, play state, position, session list); callers
/// still re-check on a timer, so a missed event only delays an update.
pub struct Watcher {
    #[cfg(windows)]
    inner: windows_impl::Watcher,
    #[cfg(not(windows))]
    _never: std::convert::Infallible,
}

impl Watcher {
    /// `None` when the platform has no media-session API (or it failed to
    /// initialise — the caller retries later).
    pub fn new() -> Option<Self> {
        #[cfg(windows)]
        {
            windows_impl::Watcher::new().map(|inner| Self { inner })
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Block up to `timeout`; `true` if a change event arrived.
    pub fn wait(&mut self, timeout: Duration) -> bool {
        #[cfg(windows)]
        {
            self.inner.wait(timeout)
        }
        #[cfg(not(windows))]
        {
            let _ = timeout;
            match self._never {}
        }
    }

    /// Current snapshot of the chosen session, `None` if nothing.
    pub fn snapshot(&mut self) -> Option<NowPlaying> {
        #[cfg(windows)]
        {
            self.inner.snapshot()
        }
        #[cfg(not(windows))]
        {
            match self._never {}
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::{
        exe_stem, pick_session, Artwork, NowPlaying, PlaybackState, SessionCandidate, Timeline,
        ARTWORK_MAX_BYTES, ARTWORK_MAX_DIM,
    };
    use crossbeam_channel::{unbounded, Receiver, Sender};
    use log::debug;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows::Foundation::{EventRegistrationToken as Token, TypedEventHandler};
    use windows::Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapEncoder, BitmapInterpolationMode, BitmapPixelFormat,
    };
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as SessionManager,
        GlobalSystemMediaTransportControlsSessionMediaProperties as MediaProperties,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    };
    use windows::Storage::Streams::{
        DataReader, IInputStream, IRandomAccessStream, InMemoryRandomAccessStream,
    };

    /// Cached cover art is re-read after this long even without an event.
    const ART_CACHE_TTL: Duration = Duration::from_secs(30);
    /// Thumbnail streams beyond this are not even read.
    const ART_READ_LIMIT: u64 = 16 * 1024 * 1024;

    /// Event kinds carried on the watcher channel (bit flags).
    const EV_MEDIA: u8 = 1; // media properties (maybe the thumbnail) changed
    const EV_SESSIONS: u8 = 2; // session list / current session changed
    const EV_OTHER: u8 = 4; // playback state or timeline

    pub struct Watcher {
        manager: SessionManager,
        tx: Sender<u8>,
        rx: Receiver<u8>,
        /// (is `SessionsChanged`, token) — else `CurrentSessionChanged`.
        manager_tokens: Vec<(bool, Token)>,
        sessions: Vec<(Session, Vec<Token>)>,
        sessions_dirty: bool,
        /// (app id + text key, art, read at) — the thumbnail is only
        /// re-read when the media properties changed or the cache aged.
        art_cache: Option<(String, Option<Artwork>, Instant)>,
    }

    impl Watcher {
        pub fn new() -> Option<Self> {
            let manager = match SessionManager::RequestAsync().and_then(|op| op.get()) {
                Ok(m) => m,
                Err(e) => {
                    debug!("now playing: media session manager unavailable: {e}");
                    return None;
                }
            };
            let (tx, rx) = unbounded();
            let mut manager_tokens = Vec::new();
            let t = tx.clone();
            if let Ok(tok) = manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
                let _ = t.send(EV_SESSIONS);
                Ok(())
            })) {
                manager_tokens.push((true, tok));
            }
            let t = tx.clone();
            if let Ok(tok) = manager.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                let _ = t.send(EV_SESSIONS);
                Ok(())
            })) {
                manager_tokens.push((false, tok));
            }
            let mut w = Self {
                manager,
                tx,
                rx,
                manager_tokens,
                sessions: Vec::new(),
                sessions_dirty: true,
                art_cache: None,
            };
            w.resubscribe_sessions();
            Some(w)
        }

        pub fn wait(&mut self, timeout: Duration) -> bool {
            let Ok(first) = self.rx.recv_timeout(timeout) else {
                return false;
            };
            let mut kinds = first;
            // Coalesce a burst (apps fill the fields in one at a time).
            std::thread::sleep(Duration::from_millis(300));
            while let Ok(k) = self.rx.try_recv() {
                kinds |= k;
            }
            if kinds & EV_MEDIA != 0 {
                self.art_cache = None;
            }
            if kinds & EV_SESSIONS != 0 {
                // New/closed sessions need fresh per-session handlers.
                self.sessions_dirty = true;
            }
            true
        }

        /// Drop the per-session handlers and register on the current
        /// session list.
        fn resubscribe_sessions(&mut self) {
            for (s, toks) in self.sessions.drain(..) {
                unsubscribe(&s, toks);
            }
            let Ok(list) = self.manager.GetSessions() else {
                return;
            };
            for s in list {
                let mut toks = Vec::with_capacity(3);
                let t = self.tx.clone();
                match s.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = t.send(EV_MEDIA);
                    Ok(())
                })) {
                    Ok(tok) => toks.push(tok),
                    Err(_) => {
                        self.sessions.push((s, toks));
                        continue;
                    }
                }
                let t = self.tx.clone();
                match s.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = t.send(EV_OTHER);
                    Ok(())
                })) {
                    Ok(tok) => toks.push(tok),
                    Err(_) => {
                        self.sessions.push((s, toks));
                        continue;
                    }
                }
                let t = self.tx.clone();
                if let Ok(tok) = s.TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = t.send(EV_OTHER);
                    Ok(())
                })) {
                    toks.push(tok);
                }
                self.sessions.push((s, toks));
            }
            self.sessions_dirty = false;
        }

        pub fn snapshot(&mut self) -> Option<NowPlaying> {
            if self.sessions_dirty {
                self.resubscribe_sessions();
            }
            let sessions: Vec<Session> = self.manager.GetSessions().ok()?.into_iter().collect();
            if sessions.is_empty() {
                return None;
            }
            let current_id = self
                .manager
                .GetCurrentSession()
                .ok()
                .and_then(|s| s.SourceAppUserModelId().ok())
                .map(|h| h.to_string_lossy());
            let cands: Vec<SessionCandidate> = sessions
                .iter()
                .map(|s| {
                    let app_id = s
                        .SourceAppUserModelId()
                        .map(|h| h.to_string_lossy())
                        .unwrap_or_default();
                    let state = s
                        .GetPlaybackInfo()
                        .and_then(|p| p.PlaybackStatus())
                        .map(map_status)
                        .unwrap_or_default();
                    let is_current = current_id.as_deref() == Some(app_id.as_str());
                    SessionCandidate { app_id, state, is_current }
                })
                .collect();
            let playing = cands.iter().filter(|c| c.state == PlaybackState::Playing).count();
            // Only worth asking the audio stack when there's a tie to break.
            let routed = if playing > 1 { routing::apps_on_our_endpoint() } else { Vec::new() };
            let idx = pick_session(&cands, &routed)?;
            let session = &sessions[idx];
            let app_id = &cands[idx].app_id;

            let props = session.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
            let text = |h: windows::core::Result<windows::core::HSTRING>| {
                h.map(|h| h.to_string_lossy().trim().to_string()).unwrap_or_default()
            };
            let mut np = NowPlaying {
                title: text(props.Title()),
                artist: text(props.Artist()),
                album: text(props.AlbumTitle()),
                artwork: None,
                timeline: timeline(session),
                state: cands[idx].state,
            };
            if np.is_empty() {
                return None;
            }

            let key = format!("{app_id}\u{1}{}\u{1}{}\u{1}{}", np.title, np.artist, np.album);
            let cached = self
                .art_cache
                .as_ref()
                .filter(|(k, _, at)| *k == key && at.elapsed() < ART_CACHE_TTL)
                .map(|(_, art, _)| art.clone());
            np.artwork = match cached {
                Some(art) => art,
                None => {
                    let art = read_thumbnail(&props);
                    self.art_cache = Some((key, art.clone(), Instant::now()));
                    art
                }
            };
            Some(np)
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            for (sessions_changed, tok) in self.manager_tokens.drain(..) {
                let _ = if sessions_changed {
                    self.manager.RemoveSessionsChanged(tok)
                } else {
                    self.manager.RemoveCurrentSessionChanged(tok)
                };
            }
            for (s, toks) in self.sessions.drain(..) {
                unsubscribe(&s, toks);
            }
        }
    }

    /// Remove the handlers [`Watcher::resubscribe_sessions`] registered,
    /// in registration order (media properties, playback, timeline).
    fn unsubscribe(s: &Session, toks: Vec<Token>) {
        for (i, tok) in toks.into_iter().enumerate() {
            let _ = match i {
                0 => s.RemoveMediaPropertiesChanged(tok),
                1 => s.RemovePlaybackInfoChanged(tok),
                _ => s.RemoveTimelinePropertiesChanged(tok),
            };
        }
    }

    fn map_status(s: Status) -> PlaybackState {
        match s {
            Status::Playing => PlaybackState::Playing,
            Status::Paused => PlaybackState::Paused,
            Status::Stopped | Status::Closed => PlaybackState::Stopped,
            _ => PlaybackState::Other,
        }
    }

    /// Track position/length, if the app reports a length. WinRT times are
    /// 100 ns ticks; `LastUpdatedTime` is a FILETIME-epoch instant telling
    /// when `Position` was sampled.
    fn timeline(session: &Session) -> Option<Timeline> {
        let tl = session.GetTimelineProperties().ok()?;
        let start = tl.StartTime().ok()?.Duration;
        let end = tl.EndTime().ok()?.Duration;
        let pos = tl.Position().ok()?.Duration;
        if end <= start {
            return None;
        }
        let duration_ms = ((end - start) / 10_000) as u64;
        let position_ms = (((pos - start).max(0)) / 10_000) as u64;
        let mut captured = Instant::now();
        if let Ok(updated) = tl.LastUpdatedTime() {
            // FILETIME epoch (1601) → Unix epoch, in 100 ns ticks.
            const EPOCH_DELTA: i64 = 116_444_736_000_000_000;
            let now_ticks = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| (d.as_nanos() / 100) as i64 + EPOCH_DELTA)
                .unwrap_or(0);
            let age = now_ticks - updated.UniversalTime;
            // Ignore nonsense (unset, in the future, or over a day old).
            if updated.UniversalTime > 0 && (0..864_000_000_000).contains(&age) {
                if let Some(t) = captured.checked_sub(Duration::from_nanos(age as u64 * 100)) {
                    captured = t;
                }
            }
        }
        Some(Timeline { position_ms: position_ms.min(duration_ms), duration_ms, captured })
    }

    /// The session's thumbnail as JPEG/PNG bytes, re-encoded if it is
    /// large or in another format. Any failure → no artwork.
    fn read_thumbnail(props: &MediaProperties) -> Option<Artwork> {
        let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
        let size = stream.Size().ok()?;
        if size == 0 || size > ART_READ_LIMIT {
            return None;
        }
        let input: IInputStream = stream.GetInputStreamAt(0).ok()?;
        let bytes = read_all(&input, size as u32)?;
        if bytes.len() <= ARTWORK_MAX_BYTES {
            if let Some(art) = Artwork::from_bytes(bytes) {
                return Some(art);
            }
        }
        let ras: IRandomAccessStream = windows::core::Interface::cast(&stream).ok()?;
        match reencode_jpeg(&ras) {
            Ok(b) => Artwork::from_bytes(b).filter(|a| a.bytes.len() <= ARTWORK_MAX_BYTES),
            Err(e) => {
                debug!("now playing: artwork re-encode failed: {e}");
                None
            }
        }
    }

    fn read_all(input: &IInputStream, len: u32) -> Option<Vec<u8>> {
        let reader = DataReader::CreateDataReader(input).ok()?;
        let n = reader.LoadAsync(len).ok()?.get().ok()?;
        if n < len {
            return None; // short read: a truncated image would still sniff as valid
        }
        let mut buf = vec![0u8; n as usize];
        reader.ReadBytes(&mut buf).ok()?;
        Some(buf)
    }

    /// Decode any image Windows can read and encode it as a JPEG whose
    /// longest edge is at most [`ARTWORK_MAX_DIM`].
    fn reencode_jpeg(src: &IRandomAccessStream) -> windows::core::Result<Vec<u8>> {
        src.Seek(0)?;
        let decoder = BitmapDecoder::CreateAsync(src)?.get()?;
        let (w, h) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
        let bitmap = decoder
            .GetSoftwareBitmapConvertedAsync(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Ignore)?
            .get()?;
        let out = InMemoryRandomAccessStream::new()?;
        let encoder = BitmapEncoder::CreateAsync(BitmapEncoder::JpegEncoderId()?, &out)?.get()?;
        encoder.SetSoftwareBitmap(&bitmap)?;
        let longest = w.max(h).max(1);
        if longest > ARTWORK_MAX_DIM {
            let t = encoder.BitmapTransform()?;
            t.SetScaledWidth(((w as u64 * ARTWORK_MAX_DIM as u64) / longest as u64).max(1) as u32)?;
            t.SetScaledHeight(((h as u64 * ARTWORK_MAX_DIM as u64) / longest as u64).max(1) as u32)?;
            t.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
        }
        encoder.FlushAsync()?.get()?;
        let size = out.Size()? as u32;
        let input = out.GetInputStreamAt(0)?;
        read_all(&input, size).ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
    }

    /// Which apps currently have active audio on the Stream To Speaker
    /// endpoint — used to tell which of several playing media sessions is
    /// the one the speaker is actually hearing.
    mod routing {
        use super::exe_stem;
        use windows::core::{Interface, HSTRING, PWSTR};
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Media::Audio::{
            AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
            IMMDeviceEnumerator, MMDeviceEnumerator,
        };
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };

        /// Lower-case exe stems; empty on any failure (no endpoint, no
        /// COM, …), which simply means "no routing hint".
        pub fn apps_on_our_endpoint() -> Vec<String> {
            let Ok(Some(id)) = crate::endpoint_name::find_our_endpoint_id() else {
                return Vec::new();
            };
            unsafe { sessions_on(&id) }.unwrap_or_default()
        }

        unsafe fn sessions_on(endpoint_id: &str) -> windows::core::Result<Vec<String>> {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDevice(&HSTRING::from(endpoint_id))?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions = manager.GetSessionEnumerator()?;
            let mut out = Vec::new();
            for i in 0..sessions.GetCount()? {
                let Ok(ctl) = sessions.GetSession(i) else { continue };
                let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
                if ctl2.GetState().ok() != Some(AudioSessionStateActive) {
                    continue;
                }
                let Ok(pid) = ctl2.GetProcessId() else { continue };
                if let Some(path) = process_image(pid) {
                    out.push(exe_stem(&path));
                }
            }
            Ok(out)
        }

        unsafe fn process_image(pid: u32) -> Option<String> {
            if pid == 0 {
                return None;
            }
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
            let _ = CloseHandle(h);
            ok.ok()?;
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(title: &str) -> NowPlaying {
        NowPlaying {
            title: title.into(),
            artist: "Artist".into(),
            state: PlaybackState::Playing,
            ..Default::default()
        }
    }

    fn with_timeline(mut n: NowPlaying, pos: u64, dur: u64, at: Instant) -> NowPlaying {
        n.timeline = Some(Timeline { position_ms: pos, duration_ms: dur, captured: at });
        n
    }

    fn art(bytes: &[u8]) -> Artwork {
        Artwork::from_bytes(bytes.to_vec()).unwrap()
    }

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];
    const JPEG2: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 9, 9, 9];
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];

    #[test]
    fn empty_detection() {
        assert!(NowPlaying::default().is_empty());
        assert!(!np("Song").is_empty());
    }

    #[test]
    fn image_sniffing() {
        assert_eq!(sniff_image_mime(JPEG), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(PNG), Some("image/png"));
        assert_eq!(sniff_image_mime(b"GIF89a"), None);
        assert_eq!(sniff_image_mime(&[]), None);
        assert!(Artwork::from_bytes(b"BM....".to_vec()).is_none());
        assert_ne!(art(JPEG).fingerprint(), art(JPEG2).fingerprint());
        assert_eq!(art(JPEG).fingerprint(), art(JPEG).fingerprint());
    }

    #[test]
    fn position_extrapolates_only_while_playing_and_clamps() {
        let t0 = Instant::now();
        let mut n = with_timeline(np("S"), 10_000, 60_000, t0);
        assert_eq!(n.position_ms_at(t0 + Duration::from_secs(5)), Some(15_000));
        assert_eq!(n.position_ms_at(t0 + Duration::from_secs(500)), Some(60_000));
        n.state = PlaybackState::Paused;
        assert_eq!(n.position_ms_at(t0 + Duration::from_secs(5)), Some(10_000));
        assert_eq!(np("S").position_ms_at(t0), None);
    }

    #[test]
    fn display_line_forms() {
        assert_eq!(np("Song").display_line(), "Artist - Song");
        let mut n = np("Song");
        n.artist.clear();
        assert_eq!(n.display_line(), "Song");
    }

    fn cand(app: &str, state: PlaybackState, current: bool) -> SessionCandidate {
        SessionCandidate { app_id: app.into(), state, is_current: current }
    }

    #[test]
    fn playing_session_beats_paused_current() {
        let c = [
            cand("Spotify.exe", PlaybackState::Paused, true),
            cand("Chrome", PlaybackState::Playing, false),
        ];
        assert_eq!(pick_session(&c, &[]), Some(1));
    }

    #[test]
    fn several_playing_prefers_routed_then_current() {
        let c = [
            cand("Chrome", PlaybackState::Playing, true),
            cand("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify", PlaybackState::Playing, false),
        ];
        // No routing info → Windows' current one.
        assert_eq!(pick_session(&c, &[]), Some(0));
        // Spotify is the one with audio on our endpoint.
        assert_eq!(pick_session(&c, &["spotify".into()]), Some(1));
        // Routing info that matches nothing playing is ignored.
        assert_eq!(pick_session(&c, &["vlc".into()]), Some(0));
        // Neither current → first playing.
        let c2 = [
            cand("A.exe", PlaybackState::Playing, false),
            cand("B.exe", PlaybackState::Playing, false),
        ];
        assert_eq!(pick_session(&c2, &[]), Some(0));
    }

    #[test]
    fn nothing_playing_falls_back_to_current_or_paused() {
        let c = [
            cand("A", PlaybackState::Stopped, false),
            cand("B", PlaybackState::Paused, false),
        ];
        assert_eq!(pick_session(&c, &[]), Some(1));
        let c = [cand("A", PlaybackState::Stopped, true), cand("B", PlaybackState::Paused, false)];
        assert_eq!(pick_session(&c, &[]), Some(0));
        assert_eq!(pick_session(&[], &[]), None);
    }

    #[test]
    fn app_matching_and_exe_stems() {
        assert_eq!(exe_stem(r"C:\Program Files\Google\Chrome\Application\chrome.exe"), "chrome");
        assert_eq!(exe_stem("Spotify.exe"), "spotify");
        assert!(app_matches_any("Spotify.exe", &["spotify".into()]));
        assert!(app_matches_any("Chrome", &["chrome".into()]));
        assert!(app_matches_any("MSEdge", &["msedge".into()]));
        assert!(!app_matches_any("Chrome", &["msedge".into()]));
        // Very short stems are too ambiguous to match on.
        assert!(!app_matches_any("Chrome", &["c".into()]));
    }

    #[test]
    fn plan_first_send_includes_everything_available() {
        let now = Instant::now();
        let mut n = with_timeline(np("S"), 0, 60_000, now);
        n.artwork = Some(art(JPEG));
        let p = Sent::default().plan(&n, now);
        assert_eq!(p, Plan { text: true, artwork: true, progress: true });
        // Nothing at all for an empty snapshot.
        assert!(Sent::default().plan(&NowPlaying::default(), now).is_empty());
    }

    #[test]
    fn plan_unchanged_sends_nothing_and_new_track_resends_all() {
        let t0 = Instant::now();
        let mut n = with_timeline(np("S1"), 0, 60_000, t0);
        n.artwork = Some(art(JPEG));
        let mut sent = Sent::default();
        let p = sent.plan(&n, t0);
        sent.record(&n, p, t0);
        let later = t0 + Duration::from_secs(10);
        assert!(sent.plan(&n, later).is_empty(), "steady playback = no traffic");
        // Next track on the same album: same art, but it is re-sent.
        let mut n2 = with_timeline(np("S2"), 0, 70_000, later);
        n2.artwork = Some(art(JPEG));
        assert_eq!(sent.plan(&n2, later), Plan { text: true, artwork: true, progress: true });
    }

    #[test]
    fn plan_late_artwork_is_sent_alone() {
        let t0 = Instant::now();
        let n = np("S");
        let mut sent = Sent::default();
        let p = sent.plan(&n, t0);
        assert_eq!(p, Plan { text: true, artwork: false, progress: false });
        sent.record(&n, p, t0);
        let mut with_art = n.clone();
        with_art.artwork = Some(art(PNG));
        assert_eq!(sent.plan(&with_art, t0), Plan { artwork: true, ..Default::default() });
        sent.record(&with_art, Plan { artwork: true, ..Default::default() }, t0);
        assert!(sent.plan(&with_art, t0).is_empty());
    }

    #[test]
    fn plan_progress_on_seek_and_resume_only() {
        let t0 = Instant::now();
        let n = with_timeline(np("S"), 0, 300_000, t0);
        let mut sent = Sent::default();
        let p = sent.plan(&n, t0);
        sent.record(&n, p, t0);

        // Small jitter: no resend.
        let t1 = t0 + Duration::from_secs(30);
        let jitter = with_timeline(np("S"), 31_000, 300_000, t1);
        assert!(!sent.plan(&jitter, t1).progress);

        // Seek forward a minute: resend.
        let seek = with_timeline(np("S"), 90_000, 300_000, t1);
        assert!(sent.plan(&seek, t1).progress);

        // Pause at 30 s: nothing to send (no way to express it) …
        let mut paused = with_timeline(np("S"), 30_000, 300_000, t1);
        paused.state = PlaybackState::Paused;
        let p = sent.plan(&paused, t1);
        assert!(p.is_empty());
        sent.record(&paused, p, t1);
        // … still nothing while it stays paused …
        let t2 = t1 + Duration::from_secs(60);
        let mut still = paused.clone();
        still.timeline = Some(Timeline { captured: t2, ..still.timeline.unwrap() });
        assert!(sent.plan(&still, t2).is_empty());
        // … and resuming re-sends the position.
        let resumed = with_timeline(np("S"), 30_000, 300_000, t2);
        assert!(sent.plan(&resumed, t2).progress);
    }

    #[test]
    fn plan_mask_drops_unsupported_parts() {
        let p = Plan { text: true, artwork: true, progress: true };
        assert_eq!(p.masked(true, false, false), Plan { text: true, ..Default::default() });
        assert!(p.masked(false, false, false).is_empty());
    }
}
