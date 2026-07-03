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

/// Current now-playing snapshot, or `None` if nothing is playing / the
/// platform doesn't support it.
#[cfg(windows)]
pub fn current() -> Option<NowPlaying> {
    windows_impl::current()
}

#[cfg(not(windows))]
pub fn current() -> Option<NowPlaying> {
    None
}

#[cfg(windows)]
mod windows_impl {
    use super::NowPlaying;
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as SessionManager;

    /// Best-effort read via the WinRT SMTC. Any failure → `None`.
    pub fn current() -> Option<NowPlaying> {
        // RequestAsync / TryGetMediaPropertiesAsync return IAsyncOperations
        // we block on with `.get()`. GetCurrentSession returns null when no
        // app has media focus — the subsequent call then errors, which the
        // `.ok()?` chain turns into `None`.
        let manager = SessionManager::RequestAsync().ok()?.get().ok()?;
        let session = manager.GetCurrentSession().ok()?;
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
