//! Persisted user preferences.
//!
//! Tiny JSON file at `%APPDATA%\StreamToSpeaker\config.json` that
//! survives across launches. Currently holds:
//!   - `last_speaker_id`: the stable id of the speaker the user last
//!     explicitly selected. Used to auto-reconnect on next launch.
//!     `None` on first launch (or after the user clicks "Forget
//!     speaker"), which is what causes the onboarding card to show.
//!   - `onboarding_dismissed`: whether the user clicked "Got it" on
//!     the onboarding card. Persisted so we don't re-show it on
//!     subsequent launches.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default)]
    pub last_speaker_id: Option<String>,
    #[serde(default)]
    pub onboarding_dismissed: bool,
    /// User ticked "Always minimise to tray" in the close-confirm
    /// modal. When true, future window-close events skip the modal
    /// and silently minimise. Inconsistency with onboarding_dismissed
    /// — which IS persisted — was confusing; this brings the close
    /// preference into the same store.
    #[serde(default)]
    pub always_minimise_to_tray: bool,
    /// Whether to auto-reconnect to `last_speaker_id` on launch.
    /// `true` (default) preserves the prior behaviour. `false` lets
    /// a user keep their saved speaker remembered (so the GUI knows
    /// what to highlight, the Forget button has something to clear)
    /// without auto-binding at startup.
    #[serde(default = "default_auto_reconnect")]
    pub auto_reconnect_on_launch: bool,
    /// AirPlay 2 stream-mode experiment switch: `true` skips buffered
    /// (type 103) and uses the low-latency realtime stream (type 96)
    /// even on receivers that advertise buffered support. Realtime is
    /// architecturally ~250 ms vs buffered's 1-2 s, but some receivers
    /// (current Sonos fw) appear to only actually *play* buffered.
    /// Edit config.json by hand to flip; no GUI yet.
    #[serde(default)]
    pub prefer_realtime_airplay: bool,
    /// Auto-reconnect when a live session drops mid-stream (speaker
    /// rebooted, Wi-Fi blip, receiver reaped the session). One retry per
    /// drop, 5 s after detection — OwnTone's field-proven policy (its 5 s
    /// spacing also respects the Sonos half-open-session hold). The dead
    /// session is torn down either way so the UI never shows a zombie
    /// "streaming" state.
    #[serde(default = "default_auto_reconnect")]
    pub auto_reconnect_on_drop: bool,
    /// RAOP `et=4` MFi-encryption experiment switch. iTunes encrypts
    /// audio to et=4 receivers with a key wrapped via the auth-setup
    /// ECDH secret; our wrap is a best-grounded guess (no open-source
    /// reference exists) whose failure can wedge the receiver for tens
    /// of seconds, so the attempt is opt-in. When enabled the session
    /// tries MFi first and falls back to plaintext/RSA on failure.
    #[serde(default)]
    pub airplay_mfi_encryption: bool,
    /// Per-device AirPlay passwords for `pw=true` receivers, keyed by the
    /// device's stable id (`airplay:<mac>`). Stored so the user only
    /// enters it once. Plain-text in the config file (same trust level as
    /// the rest of the file); RTSP Digest never sends it in the clear.
    #[serde(default)]
    pub airplay_passwords: HashMap<String, String>,
    /// Forward Windows' "now playing" (title/artist/album from the System
    /// Media Transport Controls) to the speaker as track metadata, so it
    /// shows on the speaker's display / app. **Off by default** — it's a
    /// nicety, it reads whatever app currently has media focus, and the
    /// RAOP metadata path is best-effort (a receiver that ignores it is
    /// harmless). RAOP only for now (Sonos-class); no AP2 metadata yet.
    #[serde(default)]
    pub forward_now_playing: bool,
    /// Debug escape hatch: send the uncompressed-ALAC escape frames
    /// instead of real compressed ALAC. Every field-proven sender
    /// (iTunes, OwnTone, AirConnect) sends compressed; this exists only
    /// to A/B against receivers that misbehave with the encoder.
    /// Edit config.json by hand to flip; no GUI.
    #[serde(default)]
    pub airplay_uncompressed_alac: bool,
}

fn default_auto_reconnect() -> bool {
    true
}

fn config_dir() -> Option<PathBuf> {
    // %APPDATA% on Windows. On non-Windows (tests, lint runs), fall
    // back to $XDG_CONFIG_HOME or $HOME/.config; this binary is
    // gated to Windows in practice but the module compiles cross-
    // platform so unit-test builds don't need a cfg fence.
    if let Ok(p) = std::env::var("APPDATA") {
        Some(PathBuf::from(p).join("StreamToSpeaker"))
    } else if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        Some(PathBuf::from(p).join("stream-to-speaker"))
    } else if let Ok(p) = std::env::var("HOME") {
        Some(PathBuf::from(p).join(".config").join("stream-to-speaker"))
    } else {
        None
    }
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.json"))
}

impl UserConfig {
    pub fn load() -> Self {
        let Some(path) = config_path() else { return Self::default(); };
        let Ok(content) = std::fs::read_to_string(&path) else { return Self::default(); };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// Best-effort save. Logged-on-failure rather than propagated — a
    /// failed config write should never block UI actions like speaker
    /// selection.
    pub fn save(&self) {
        let Some(path) = config_path() else { return; };
        let Some(dir) = path.parent() else { return; };
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::warn!("user_config: mkdir {}: {}", dir.display(), e);
            return;
        }
        let content = match serde_json::to_string_pretty(self) {
            Ok(s) => s,
            Err(e) => { log::warn!("user_config: serialize: {}", e); return; }
        };
        if let Err(e) = std::fs::write(&path, content) {
            log::warn!("user_config: write {}: {}", path.display(), e);
        }
    }
}
