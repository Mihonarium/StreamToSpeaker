//! Song titles go to a speaker only from music apps: a title from any other
//! app, a browser or a video player included, is never sent. They stop
//! when the music app pauses or stops, when this PC's sound leaves the
//! speaker, when the speaker's switch for titles is off, and while another
//! app's real sound plays too (the title would name the wrong sound).

use super::io::AppKey;

/// The music apps whose titles may be shown, by their program's name
/// (compared without case, spaces or ".exe").
pub const MUSIC_APPS: &[&str] = &[
    "Spotify",
    "Apple Music",
    "AppleMusic",
    "iTunes",
    "TIDAL",
    "Deezer",
    "Amazon Music",
    "Qobuz",
    "foobar2000",
    "MusicBee",
    "AIMP",
    "Winamp",
    "Plexamp",
    "Roon",
    "Audirvana",
    "Cider",
    "Pandora",
    "Strawberry",
    "Clementine",
    "Dopamine",
];

fn plain(name: &str) -> String {
    let lower = name.to_lowercase();
    let lower = lower.strip_suffix(".exe").unwrap_or(&lower);
    lower.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whether `app` is one of the music apps.
pub fn is_music_app(app: &AppKey) -> bool {
    let app = plain(app.as_str());
    MUSIC_APPS.iter().any(|m| plain(m) == app)
}

/// Whether the title of `app`'s player may be sent now: it is a music app,
/// its player plays, this PC's sound is on that speaker, the speaker's
/// switch is on, and no other app's real sound plays.
pub fn may_send_title(app: &AppKey, player_playing: bool, on_speaker: bool, switch_on: bool, other_real_sound: bool) -> bool {
    is_music_app(app) && player_playing && on_speaker && switch_on && !other_real_sound
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_only_from_music_apps() {
        let ok = |app: &str| may_send_title(&AppKey::new(app), true, true, true, false);
        assert!(ok("Spotify"));
        assert!(ok("Spotify.exe"), "the program's name");
        assert!(ok("spotify"));
        assert!(ok("Apple Music"));
        for app in ["chrome", "msedge.exe", "firefox", "vlc", "Microsoft.Media.Player", "Teams", ""] {
            assert!(!ok(app), "{app}: never sent");
        }
        let spotify = AppKey::new("Spotify");
        assert!(!may_send_title(&spotify, false, true, true, false), "paused or stopped");
        assert!(!may_send_title(&spotify, true, false, true, false), "the sound left the speaker");
        assert!(!may_send_title(&spotify, true, true, false, false), "the switch is off");
        assert!(!may_send_title(&spotify, true, true, true, true), "another app's real sound plays too");
    }
}
