//! Reads the OS "now playing" (title / artist / album) for the opt-in
//! metadata-forwarding feature.
//!
//! On Windows this comes from the **System Media Transport Controls**
//! (`Windows.Media.Control`) — the same source that feeds the volume-key
//! media overlay. It reflects whichever app currently holds media focus
//! (Spotify, a browser tab, a media player, …). Everything here is
//! best-effort: a WinRT hiccup, no active session, or an unsupported OS
//! just yields `None`, never an error, so the caller can poll it blindly.

/// A snapshot of what the OS reports as currently playing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
}

impl NowPlaying {
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.artist.is_empty() && self.album.is_empty()
    }
}

/// Windows' current media session: whose it is, whether it plays, and
/// its title.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    /// The app the session belongs to, as Windows names it: a program's
    /// file name (`Spotify.exe`) or a Store app's id. None where Windows
    /// doesn't say.
    pub app: Option<String>,
    /// Windows reports that the session is playing.
    pub playing: bool,
    pub title: Option<NowPlaying>,
}

#[cfg(any(windows, test))]
impl Session {
    pub(crate) fn from_report(app: Option<String>, playing: Option<bool>, title: Option<NowPlaying>) -> Self {
        Self { app, playing: playing.unwrap_or(false), title }
    }
}

/// Windows' current media session, or `None` if there is none / the
/// platform doesn't support it.
#[cfg(windows)]
pub fn current_session() -> Option<Session> {
    windows_impl::current_session()
}

#[cfg(not(windows))]
pub fn current_session() -> Option<Session> {
    None
}

#[cfg(windows)]
mod windows_impl {
    use super::{NowPlaying, Session};
    use windows::Media::Control::GlobalSystemMediaTransportControlsSession as MediaSession;
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as SessionManager;
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus;

    /// Best-effort read via the WinRT SMTC. Any failure → `None`.
    pub fn current_session() -> Option<Session> {
        // RequestAsync returns an IAsyncOperation we block on with
        // `.get()`. GetCurrentSession returns null when no app has media
        // focus — the calls on it then error, which the `.ok()?` chain
        // turns into `None`.
        let manager = SessionManager::RequestAsync().ok()?.get().ok()?;
        let session = manager.GetCurrentSession().ok()?;
        let app = session
            .SourceAppUserModelId()
            .ok()
            .map(|id| id.to_string_lossy().trim().to_string())
            .filter(|id| !id.is_empty());
        let playing = session
            .GetPlaybackInfo()
            .and_then(|info| info.PlaybackStatus())
            .ok()
            .map(|status| status == PlaybackStatus::Playing);
        Some(Session::from_report(app, playing, title_of(&session)))
    }

    fn title_of(session: &MediaSession) -> Option<NowPlaying> {
        let props = session.TryGetMediaPropertiesAsync().ok()?.get().ok()?;

        let title = props.Title().map(|h| h.to_string_lossy()).unwrap_or_default();
        let artist = props.Artist().map(|h| h.to_string_lossy()).unwrap_or_default();
        let album = props
            .AlbumTitle()
            .map(|h| h.to_string_lossy())
            .unwrap_or_default();

        let np = NowPlaying {
            title: title.trim().to_string(),
            artist: artist.trim().to_string(),
            album: album.trim().to_string(),
        };
        (!np.is_empty()).then_some(np)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_detection() {
        assert!(NowPlaying::default().is_empty());
        assert!(!NowPlaying {
            title: "Song".into(),
            ..Default::default()
        }
        .is_empty());
    }
}
